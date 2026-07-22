use std::collections::{BTreeSet, HashMap, HashSet};
use std::env;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Corpus {
    #[serde(default = "default_schema_version")]
    schema_version: u32,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    id: String,
    tags: Vec<String>,
    #[serde(default = "default_shape")]
    shape: String,
    review_input: ReviewInput,
    valid_anchors: HashMap<String, BTreeSet<u64>>,
    expected: Vec<ExpectedFinding>,
}

#[derive(Deserialize)]
struct ReviewInput {
    diff: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExpectedFinding {
    id: String,
    category: String,
    file: String,
    line: Option<u64>,
    suggestion: Option<String>,
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    equivalence_group: Option<String>,
    #[serde(default)]
    accepted_fix_patterns: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Capture {
    name: String,
    model: Option<String>,
    thinking: Option<String>,
    side_effects: bool,
    #[serde(default)]
    complete: Option<bool>,
    #[serde(default)]
    provenance: Option<Provenance>,
    runs: Vec<Run>,
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Provenance {
    workflow_commit: String,
    workflow_tree_hash: String,
    skill_hash: String,
    source_corpus_hash: String,
    review_input_hash: String,
    provider_model: String,
    thinking: String,
    tool_permissions: String,
    timeout_seconds: u64,
    environment_version: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Run {
    case_id: String,
    repetition: u32,
    findings: Vec<CapturedFinding>,
    tokens: TokenUsage,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CapturedFinding {
    category: String,
    #[serde(default)]
    severity: Option<String>,
    file: String,
    line: Option<u64>,
    title: String,
    suggestion: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct TokenUsage {
    uncached_input: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    output: Option<u64>,
    reasoning: Option<u64>,
    cost_usd: Option<f64>,
}

#[derive(Default)]
struct Score {
    true_positive: usize,
    false_positive: usize,
    false_negative: usize,
    anchors_valid: usize,
    anchors_total: usize,
    suggestions_applicable: usize,
    suggestions_total: usize,
    severity_hit: f64,
    severity_total: f64,
    critical_high_hit: usize,
    critical_high_total: usize,
    cross_file_hit: usize,
    cross_file_total: usize,
    test_gap_hit: usize,
    test_gap_total: usize,
    prs: usize,
}

#[derive(Default)]
struct TokenAggregate {
    uncached_input: Dimension,
    cache_read: Dimension,
    cache_write: Dimension,
    output: Dimension,
    reasoning: Dimension,
    cost_usd: CostDimension,
    logical_by_repetition: HashMap<u32, Dimension>,
}

#[derive(Default)]
struct Dimension {
    total: u64,
    missing: usize,
}

#[derive(Default)]
struct CostDimension {
    total: f64,
    missing: usize,
}

fn main() -> Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.len() != 3 {
        bail!(
            "usage: reviewer_benchmark <corpus.json> <pipeline-capture.json> <skill-capture.json>"
        );
    }
    let corpus: Corpus = read_json(&args[0])?;
    validate_corpus(&corpus)?;
    let pipeline: Capture = read_json(&args[1])?;
    let skill: Capture = read_json(&args[2])?;
    validate_capture(&corpus, &pipeline)?;
    validate_capture(&corpus, &skill)?;
    if pipeline.model != skill.model || pipeline.thinking != skill.thinking {
        bail!("captures must use the same model and thinking level");
    }
    if repetition_ids(&pipeline) != repetition_ids(&skill) {
        bail!("captures must contain the same repetition IDs");
    }
    validate_provenance_parity(&pipeline, &skill)?;

    println!("Reviewer benchmark (deterministic evaluator)");
    println!(
        "model={} thinking={}",
        pipeline.model.as_deref().unwrap_or("unavailable"),
        pipeline.thinking.as_deref().unwrap_or("unavailable")
    );
    evaluate_and_print(&corpus, &pipeline)?;
    evaluate_and_print(&corpus, &skill)?;
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: impl AsRef<Path>) -> Result<T> {
    let path = path.as_ref();
    let contents =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    serde_json::from_str(&contents).with_context(|| format!("could not parse {}", path.display()))
}

fn validate_corpus(corpus: &Corpus) -> Result<()> {
    let required: &[&str] = if corpus.schema_version >= 2 {
        &[
            "code-smell",
            "security",
            "performance",
            "test-quality",
            "convention",
            "whole-file-deletion",
            "cross-layer",
            "authorization-security",
            "partial-migration",
            "large-file-structure",
            "unconventional-layout",
            "weak-missing-tests",
            "dependency-change",
            "false-positive-trap",
            "finding-heavy",
        ]
    } else {
        &[
            "code-smell",
            "security",
            "performance",
            "test-quality",
            "convention",
            "deletion-only",
            "rename",
            "svg-security",
            "cross-file",
            "multiline-assertion",
            "false-positive-trap",
            "suggestion-quality",
        ]
    };
    let tags = corpus
        .cases
        .iter()
        .flat_map(|case| case.tags.iter().map(String::as_str))
        .collect::<BTreeSet<_>>();
    for required in required {
        if !tags.contains(required) {
            bail!("corpus is missing required coverage tag `{required}`");
        }
    }
    for case in &corpus.cases {
        if case.review_input.diff.trim().is_empty() {
            bail!("case `{}` has no reviewable diff", case.id);
        }
    }
    Ok(())
}

fn default_schema_version() -> u32 {
    1
}

fn default_shape() -> String {
    "fixture".to_owned()
}

fn validate_capture(corpus: &Corpus, capture: &Capture) -> Result<()> {
    if capture.complete == Some(false) {
        bail!("capture `{}` is incomplete", capture.name);
    }
    if capture.side_effects {
        bail!(
            "capture `{}` reports side effects; refusing to score it",
            capture.name
        );
    }
    if capture.runs.is_empty() {
        bail!("capture `{}` has no runs", capture.name);
    }
    let known_cases = corpus
        .cases
        .iter()
        .map(|case| case.id.as_str())
        .collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    let mut repetitions_by_case = HashMap::<&str, BTreeSet<u32>>::new();
    for run in &capture.runs {
        if !known_cases.contains(run.case_id.as_str()) {
            bail!("unknown case `{}` in {}", run.case_id, capture.name);
        }
        if run.repetition == 0 {
            bail!("capture `{}` uses repetition 0", capture.name);
        }
        if !seen.insert((run.case_id.as_str(), run.repetition)) {
            bail!(
                "capture `{}` duplicates case `{}` repetition {}",
                capture.name,
                run.case_id,
                run.repetition
            );
        }
        repetitions_by_case
            .entry(run.case_id.as_str())
            .or_default()
            .insert(run.repetition);
    }
    let expected_repetitions = repetition_ids(capture);
    for case in &corpus.cases {
        if repetitions_by_case.get(case.id.as_str()) != Some(&expected_repetitions) {
            bail!(
                "capture `{}` does not cover every repetition for case `{}`",
                capture.name,
                case.id
            );
        }
    }
    Ok(())
}

fn validate_provenance_parity(left: &Capture, right: &Capture) -> Result<()> {
    match (&left.provenance, &right.provenance) {
        (None, None) => Ok(()),
        (Some(left), Some(right)) if left == right => Ok(()),
        (Some(_), Some(_)) => bail!(
            "live captures must use identical workflow, skill, corpus, model, permissions, timeout, and environment provenance"
        ),
        _ => bail!("live captures must both include complete provenance"),
    }
}

fn repetition_ids(capture: &Capture) -> BTreeSet<u32> {
    capture.runs.iter().map(|run| run.repetition).collect()
}

fn evaluate_and_print(corpus: &Corpus, capture: &Capture) -> Result<()> {
    let cases = corpus
        .cases
        .iter()
        .map(|case| (case.id.as_str(), case))
        .collect::<HashMap<_, _>>();
    let mut score = Score::default();
    let mut tokens = TokenAggregate::default();
    let mut scores_by_shape = HashMap::<&str, Score>::new();
    let mut tokens_by_shape = HashMap::<&str, TokenAggregate>::new();
    let mut repetitions = BTreeSet::new();
    for run in &capture.runs {
        let case = cases
            .get(run.case_id.as_str())
            .with_context(|| format!("unknown case `{}` in {}", run.case_id, capture.name))?;
        repetitions.insert(run.repetition);
        score_run(case, run, &mut score);
        tokens.add(run.repetition, &run.tokens);
        score_run(case, run, scores_by_shape.entry(&case.shape).or_default());
        tokens_by_shape
            .entry(&case.shape)
            .or_default()
            .add(run.repetition, &run.tokens);
    }

    let precision = ratio(
        score.true_positive,
        score.true_positive + score.false_positive,
    );
    let recall = ratio(
        score.true_positive,
        score.true_positive + score.false_negative,
    );
    let f1 = if precision + recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    };
    println!(
        "\n{} ({} repetition set(s))",
        capture.name,
        repetitions.len()
    );
    println!(
        "accuracy: precision={precision:.3} recall={recall:.3} f1={f1:.3} severityWeightedRecall={:.3} criticalHighRecall={:.3} crossFileRecall={:.3} testGapRecall={:.3} falsePositivesPerPr={:.3} anchorValidity={:.3} suggestionCorrectness={:.3}",
        float_ratio(score.severity_hit, score.severity_total),
        ratio(score.critical_high_hit, score.critical_high_total),
        ratio(score.cross_file_hit, score.cross_file_total),
        ratio(score.test_gap_hit, score.test_gap_total),
        ratio(score.false_positive, score.prs),
        ratio(score.anchors_valid, score.anchors_total),
        ratio(score.suggestions_applicable, score.suggestions_total)
    );
    println!(
        "counts: tp={} fp={} fn={}",
        score.true_positive, score.false_positive, score.false_negative
    );
    tokens.print();
    let mut shapes = scores_by_shape.keys().copied().collect::<Vec<_>>();
    shapes.sort_unstable();
    for shape in shapes {
        let bucket = &scores_by_shape[shape];
        let precision = ratio(
            bucket.true_positive,
            bucket.true_positive + bucket.false_positive,
        );
        let recall = ratio(
            bucket.true_positive,
            bucket.true_positive + bucket.false_negative,
        );
        let f1 = if precision + recall == 0.0 {
            0.0
        } else {
            2.0 * precision * recall / (precision + recall)
        };
        println!(
            "shape.{shape}: precision={precision:.3} recall={recall:.3} f1={f1:.3} falsePositivesPerPr={:.3}",
            ratio(bucket.false_positive, bucket.prs)
        );
        tokens_by_shape[shape].print_with_prefix(&format!("shape.{shape}."));
    }
    Ok(())
}

fn score_run(case: &Case, run: &Run, score: &mut Score) {
    score.prs += 1;
    let mut matched = vec![false; case.expected.len()];
    for finding in &run.findings {
        score.anchors_total += 1;
        if finding.line.is_none()
            || case
                .valid_anchors
                .get(&finding.file)
                .is_some_and(|anchors| finding.line.is_some_and(|line| anchors.contains(&line)))
        {
            score.anchors_valid += 1;
        }
        let candidate = case
            .expected
            .iter()
            .enumerate()
            .position(|(index, expected)| {
                !matched[index]
                    && expected.category == finding.category
                    && expected.file == finding.file
                    && (expected.line.is_none() || expected.line == finding.line)
            });
        if let Some(index) = candidate {
            matched[index] = true;
            score.true_positive += 1;
            let expected = &case.expected[index];
            if expected.suggestion.is_some() || !expected.accepted_fix_patterns.is_empty() {
                score.suggestions_total += 1;
                if suggestion_correct(expected, finding.suggestion.as_deref()) {
                    score.suggestions_applicable += 1;
                }
            }
            let _ = (
                &expected.id,
                &expected.equivalence_group,
                &finding.title,
                &finding.severity,
            );
        } else {
            score.false_positive += 1;
        }
    }
    for (expected, matched) in case.expected.iter().zip(&matched) {
        let weight = severity_weight(expected.severity.as_deref());
        score.severity_total += weight;
        if *matched {
            score.severity_hit += weight;
        }
        if matches!(expected.severity.as_deref(), Some("Critical" | "High")) {
            score.critical_high_total += 1;
            score.critical_high_hit += usize::from(*matched);
        }
        if case.tags.iter().any(|tag| {
            matches!(
                tag.as_str(),
                "cross-file" | "cross-layer" | "cross-directory" | "partial-migration"
            )
        }) {
            score.cross_file_total += 1;
            score.cross_file_hit += usize::from(*matched);
        }
        if expected.category == "Test Quality" {
            score.test_gap_total += 1;
            score.test_gap_hit += usize::from(*matched);
        }
    }
    score.false_negative += matched.iter().filter(|matched| !**matched).count();
}

fn suggestion_correct(expected: &ExpectedFinding, actual: Option<&str>) -> bool {
    let Some(actual) = actual else {
        return false;
    };
    if expected
        .suggestion
        .as_deref()
        .is_some_and(|suggestion| suggestion == actual)
    {
        return true;
    }
    !expected.accepted_fix_patterns.is_empty()
        && expected
            .accepted_fix_patterns
            .iter()
            .all(|pattern| actual.contains(pattern))
}

fn severity_weight(severity: Option<&str>) -> f64 {
    match severity {
        Some("Critical") => 8.0,
        Some("High") => 4.0,
        Some("Medium") => 2.0,
        _ => 1.0,
    }
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        1.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn float_ratio(numerator: f64, denominator: f64) -> f64 {
    if denominator == 0.0 {
        1.0
    } else {
        numerator / denominator
    }
}

impl TokenAggregate {
    fn add(&mut self, repetition: u32, usage: &TokenUsage) {
        self.uncached_input.add(usage.uncached_input);
        self.cache_read.add(usage.cache_read);
        self.cache_write.add(usage.cache_write);
        self.output.add(usage.output);
        self.reasoning.add(usage.reasoning);
        self.cost_usd.add(usage.cost_usd);
        self.logical_by_repetition
            .entry(repetition)
            .or_default()
            .add(logical_tokens(usage));
    }

    fn print(&self) {
        self.print_with_prefix("");
    }

    fn print_with_prefix(&self, prefix: &str) {
        self.uncached_input.print(&format!("{prefix}uncachedInput"));
        self.cache_read.print(&format!("{prefix}cacheRead"));
        self.cache_write.print(&format!("{prefix}cacheWrite"));
        self.output.print(&format!("{prefix}output"));
        self.reasoning.print(&format!("{prefix}reasoning"));
        if self.uncached_input.missing
            + self.cache_read.missing
            + self.cache_write.missing
            + self.output.missing
            + self.reasoning.missing
            == 0
        {
            let logical = self.uncached_input.total
                + self.cache_read.total
                + self.cache_write.total
                + self.output.total
                + self.reasoning.total;
            println!("tokens.{prefix}logicalTotal={logical}");
        } else {
            println!("tokens.{prefix}logicalTotal=unavailable");
        }
        let logical_repetitions = self.logical_by_repetition.values().collect::<Vec<_>>();
        if logical_repetitions.iter().all(|value| value.missing == 0) {
            let mut totals = logical_repetitions
                .iter()
                .map(|value| value.total)
                .collect::<Vec<_>>();
            totals.sort_unstable();
            println!(
                "tokens.{prefix}medianLogicalPerRepetition={:.1}",
                median(&totals)
            );
        } else {
            println!("tokens.{prefix}medianLogicalPerRepetition=unavailable");
        }
        if self.cost_usd.missing == 0 {
            println!("{prefix}costUsd={:.6}", self.cost_usd.total);
        } else {
            println!(
                "{prefix}costUsd=unavailable ({} run(s))",
                self.cost_usd.missing
            );
        }
    }
}

fn logical_tokens(usage: &TokenUsage) -> Option<u64> {
    Some(
        usage.uncached_input?
            + usage.cache_read?
            + usage.cache_write?
            + usage.output?
            + usage.reasoning?,
    )
}

fn median(values: &[u64]) -> f64 {
    let midpoint = values.len() / 2;
    if values.len() % 2 == 0 {
        (values[midpoint - 1] as f64 + values[midpoint] as f64) / 2.0
    } else {
        values[midpoint] as f64
    }
}

impl Dimension {
    fn add(&mut self, value: Option<u64>) {
        match value {
            Some(value) => self.total += value,
            None => self.missing += 1,
        }
    }

    fn print(&self, name: &str) {
        if self.missing == 0 {
            println!("tokens.{name}={}", self.total);
        } else {
            println!("tokens.{name}=unavailable ({} run(s))", self.missing);
        }
    }
}

impl CostDimension {
    fn add(&mut self, value: Option<f64>) {
        match value {
            Some(value) => self.total += value,
            None => self.missing += 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus() -> Corpus {
        Corpus {
            schema_version: 1,
            cases: vec![Case {
                id: "case".into(),
                tags: Vec::new(),
                shape: "fixture".into(),
                review_input: ReviewInput {
                    diff: "diff".into(),
                },
                valid_anchors: HashMap::new(),
                expected: Vec::new(),
            }],
        }
    }

    fn capture(side_effects: bool, complete: Option<bool>) -> Capture {
        Capture {
            name: "capture".into(),
            model: Some("provider/model".into()),
            thinking: Some("high".into()),
            side_effects,
            complete,
            provenance: None,
            runs: vec![Run {
                case_id: "case".into(),
                repetition: 1,
                findings: Vec::new(),
                tokens: TokenUsage::default(),
            }],
        }
    }

    #[test]
    fn rejects_side_effecting_and_incomplete_captures() {
        assert!(validate_capture(&corpus(), &capture(true, Some(true))).is_err());
        assert!(validate_capture(&corpus(), &capture(false, Some(false))).is_err());
        assert!(validate_capture(&corpus(), &capture(false, Some(true))).is_ok());
    }

    #[test]
    fn live_provenance_must_match_exactly() {
        let json = r#"{
          "workflowCommit":"abc","workflowTreeHash":"tree","skillHash":"skill","sourceCorpusHash":"source",
          "reviewInputHash":"input","providerModel":"provider/model","thinking":"high",
          "toolPermissions":"read-only","timeoutSeconds":240,"environmentVersion":"env"
        }"#;
        let provenance: Provenance = serde_json::from_str(json).unwrap();
        let mut left = capture(false, Some(true));
        left.provenance = Some(provenance);
        let mut right = capture(false, Some(true));
        right.provenance = Some(serde_json::from_str(json).unwrap());
        assert!(validate_provenance_parity(&left, &right).is_ok());
        right.provenance.as_mut().unwrap().timeout_seconds = 241;
        assert!(validate_provenance_parity(&left, &right).is_err());
    }
}
