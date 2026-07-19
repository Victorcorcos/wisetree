use std::collections::{BTreeSet, HashMap, HashSet};
use std::env;
use std::fs;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Corpus {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    id: String,
    tags: Vec<String>,
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
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Capture {
    name: String,
    model: Option<String>,
    thinking: Option<String>,
    side_effects: bool,
    runs: Vec<Run>,
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
    let required = [
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
    ];
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

fn validate_capture(corpus: &Corpus, capture: &Capture) -> Result<()> {
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
    let mut repetitions = BTreeSet::new();
    for run in &capture.runs {
        let case = cases
            .get(run.case_id.as_str())
            .with_context(|| format!("unknown case `{}` in {}", run.case_id, capture.name))?;
        repetitions.insert(run.repetition);
        score_run(case, run, &mut score);
        tokens.add(run.repetition, &run.tokens);
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
        "accuracy: precision={precision:.3} recall={recall:.3} f1={f1:.3} anchorValidity={:.3} suggestionApplicability={:.3}",
        ratio(score.anchors_valid, score.anchors_total),
        ratio(score.suggestions_applicable, score.suggestions_total)
    );
    println!(
        "counts: tp={} fp={} fn={}",
        score.true_positive, score.false_positive, score.false_negative
    );
    tokens.print();
    Ok(())
}

fn score_run(case: &Case, run: &Run, score: &mut Score) {
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
                    && expected.line == finding.line
            });
        if let Some(index) = candidate {
            matched[index] = true;
            score.true_positive += 1;
            let expected = &case.expected[index];
            if let Some(suggestion) = &expected.suggestion {
                score.suggestions_total += 1;
                if finding.suggestion.as_deref() == Some(suggestion.as_str()) {
                    score.suggestions_applicable += 1;
                }
            }
            let _ = (&expected.id, &finding.title);
        } else {
            score.false_positive += 1;
        }
    }
    score.false_negative += matched.iter().filter(|matched| !**matched).count();
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        1.0
    } else {
        numerator as f64 / denominator as f64
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
        self.uncached_input.print("uncachedInput");
        self.cache_read.print("cacheRead");
        self.cache_write.print("cacheWrite");
        self.output.print("output");
        self.reasoning.print("reasoning");
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
            println!("tokens.logicalTotal={logical}");
        } else {
            println!("tokens.logicalTotal=unavailable");
        }
        let logical_repetitions = self.logical_by_repetition.values().collect::<Vec<_>>();
        if logical_repetitions.iter().all(|value| value.missing == 0) {
            let mut totals = logical_repetitions
                .iter()
                .map(|value| value.total)
                .collect::<Vec<_>>();
            totals.sort_unstable();
            println!("tokens.medianLogicalPerRepetition={:.1}", median(&totals));
        } else {
            println!("tokens.medianLogicalPerRepetition=unavailable");
        }
        if self.cost_usd.missing == 0 {
            println!("costUsd={:.6}", self.cost_usd.total);
        } else {
            println!("costUsd=unavailable ({} run(s))", self.cost_usd.missing);
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
