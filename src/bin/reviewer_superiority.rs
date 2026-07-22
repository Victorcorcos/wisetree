use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Preregistration {
    baseline_model: String,
    baseline_thinking: String,
    minimum_repetitions: u32,
    bootstrap_resamples: usize,
    bootstrap_seed: u64,
    confidence_level: f64,
    thresholds: Thresholds,
    confidence_gates: ConfidenceGates,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Thresholds {
    f1_delta: f64,
    recall_delta: f64,
    precision_delta: f64,
    critical_high_recall: f64,
    critical_high_recall_delta: f64,
    suggestion_success_delta: f64,
    median_logical_token_reduction: f64,
    maximum_shape_token_regression: f64,
    minimum_semantic_agreement: f64,
    minimum_cohen_kappa: f64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfidenceGates {
    f1_delta_lower_bound: f64,
    recall_delta_lower_bound: f64,
    logical_token_reduction_lower_bound: f64,
}

#[derive(Debug, Deserialize)]
struct Corpus {
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    id: String,
    shape: String,
    valid_anchors: HashMap<String, BTreeSet<u64>>,
    expected: Vec<ExpectedFinding>,
}

#[derive(Debug, Deserialize)]
struct ExpectedFinding {
    category: String,
    severity: Option<String>,
    file: String,
    line: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Capture {
    name: String,
    model: Option<String>,
    thinking: Option<String>,
    side_effects: bool,
    complete: Option<bool>,
    provenance: Provenance,
    runs: Vec<Run>,
}

#[derive(Debug, Deserialize, PartialEq)]
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Run {
    case_id: String,
    repetition: u32,
    findings: Vec<Finding>,
    tokens: Tokens,
}

#[derive(Debug, Deserialize, Serialize)]
struct Finding {
    category: String,
    file: String,
    line: Option<u64>,
    suggestion: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Tokens {
    uncached_input: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    output: Option<u64>,
    reasoning: Option<u64>,
}

impl Tokens {
    fn logical(&self) -> Option<u64> {
        Some(
            self.uncached_input?
                + self.cache_read?
                + self.cache_write?
                + self.output?
                + self.reasoning?,
        )
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdjudicationReport {
    packet_hash: String,
    blind: bool,
    candidate_count: usize,
    raw_semantic_agreement: f64,
    cohen_kappa_semantic: f64,
    disagreements: usize,
    resolved_disagreements: usize,
    decisions: Vec<Decision>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Decision {
    candidate_id: String,
    semantically_correct: bool,
    severity_correct: bool,
    duplicate_group: Option<String>,
    fix_semantically_correct: Option<bool>,
    application: ValidationOutcome,
    formatter: ValidationOutcome,
    parser: ValidationOutcome,
    build: ValidationOutcome,
    tests: ValidationOutcome,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum ValidationOutcome {
    Pass,
    Fail,
    NotApplicable,
    Unavailable,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrivateMap {
    packet_hash: String,
    candidates: Vec<CandidateOrigin>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CandidateOrigin {
    candidate_id: String,
    workflow: String,
    case_id: String,
    repetition: u32,
    finding_index: usize,
}

#[derive(Debug, Clone, Copy, Default)]
struct Score {
    tp: usize,
    fp: usize,
    fn_: usize,
    high_tp: usize,
    high_total: usize,
    anchors_valid: usize,
    anchors_total: usize,
    duplicates: usize,
}

impl Score {
    fn precision(self) -> f64 {
        ratio(self.tp, self.tp + self.fp)
    }

    fn recall(self) -> f64 {
        ratio(self.tp, self.tp + self.fn_)
    }

    fn f1(self) -> f64 {
        let precision = self.precision();
        let recall = self.recall();
        if precision + recall == 0.0 {
            0.0
        } else {
            2.0 * precision * recall / (precision + recall)
        }
    }

    fn high_recall(self) -> f64 {
        ratio(self.high_tp, self.high_total)
    }

    fn add(&mut self, other: Self) {
        self.tp += other.tp;
        self.fp += other.fp;
        self.fn_ += other.fn_;
        self.high_tp += other.high_tp;
        self.high_total += other.high_total;
        self.anchors_valid += other.anchors_valid;
        self.anchors_total += other.anchors_total;
        self.duplicates += other.duplicates;
    }
}

#[derive(Debug)]
struct Pair {
    shape: String,
    review: Score,
    skill: Score,
    review_tokens: u64,
    skill_tokens: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaimStatus {
    schema_version: u32,
    claim_enabled: bool,
    stale: bool,
    baseline_key: Option<String>,
    evaluated_at_ms: Option<u64>,
    reason: String,
    gates: Vec<Gate>,
    report: Option<Report>,
}

#[derive(Debug, Serialize)]
struct Gate {
    name: String,
    passed: bool,
    observed: String,
    required: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Report {
    discovery: WorkflowComparison,
    delivery: DeliveryComparison,
    token_reduction: f64,
    token_reduction_by_shape: BTreeMap<String, f64>,
    suggestion_success_delta: f64,
    confidence_intervals: ConfidenceIntervals,
    evidence_packet_hash: String,
}

#[derive(Debug, Serialize)]
struct WorkflowComparison {
    review: Accuracy,
    skill: Accuracy,
    delta: Accuracy,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Accuracy {
    precision: f64,
    recall: f64,
    f1: f64,
    critical_high_recall: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeliveryComparison {
    review_anchor_validity: f64,
    skill_anchor_validity: f64,
    review_duplicate_findings: usize,
    skill_duplicate_findings: usize,
    review_severity_accuracy: f64,
    skill_severity_accuracy: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfidenceIntervals {
    f1_delta: [f64; 2],
    recall_delta: [f64; 2],
    logical_token_reduction: [f64; 2],
}

fn main() -> Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.len() == 2 && args[0] == "check-status" {
        return check_status(Path::new(&args[1]));
    }
    if args.len() == 6 && args[0] == "nightly" {
        return nightly_guard(
            Path::new(&args[1]),
            Path::new(&args[2]),
            Path::new(&args[3]),
            Path::new(&args[4]),
            Path::new(&args[5]),
        );
    }
    if args.len() != 8 || args[0] != "gate" {
        bail!("usage: reviewer_superiority gate <preregistration> <corpus> <review-capture> <skill-capture> <adjudication-report> <private-map> <status-output> | reviewer_superiority check-status <status>");
    }
    let output = Path::new(&args[7]);
    match run_gate(
        Path::new(&args[1]),
        Path::new(&args[2]),
        Path::new(&args[3]),
        Path::new(&args[4]),
        Path::new(&args[5]),
        Path::new(&args[6]),
    ) {
        Ok(status) => {
            write_json(output, &status)?;
            if !status.claim_enabled {
                bail!(
                    "joint-superiority claim remains disabled; see {}",
                    output.display()
                );
            }
            Ok(())
        }
        Err(error) => {
            let status = ClaimStatus {
                schema_version: 1,
                claim_enabled: false,
                stale: false,
                baseline_key: None,
                evaluated_at_ms: Some(unix_millis()),
                reason: format!("invalid comparison: {error:#}"),
                gates: Vec::new(),
                report: None,
            };
            write_json(output, &status)?;
            Err(error)
        }
    }
}

fn nightly_guard(
    prereg_path: &Path,
    corpus_path: &Path,
    review_path: &Path,
    skill_path: &Path,
    output: &Path,
) -> Result<()> {
    let prereg: Preregistration = read_json(prereg_path)?;
    let corpus: Corpus = read_json(corpus_path)?;
    let review: Capture = read_json(review_path)?;
    let skill: Capture = read_json(skill_path)?;
    if review.side_effects
        || skill.side_effects
        || review.complete != Some(true)
        || skill.complete != Some(true)
        || review.provenance != skill.provenance
        || review.model != skill.model
        || review.thinking != skill.thinking
    {
        bail!("nightly captures are incomplete, side-effecting, or provenance-mismatched");
    }
    if review.model.as_deref() != Some(prereg.baseline_model.as_str())
        || review.thinking.as_deref() != Some(prereg.baseline_thinking.as_str())
    {
        bail!("nightly provider/model or thinking changed; preregister a new baseline");
    }
    let pairs = paired_units(&corpus, &review, &skill, prereg.minimum_repetitions)?;
    let review_score = aggregate_score(&pairs, true);
    let skill_score = aggregate_score(&pairs, false);
    let review_accuracy = accuracy(review_score);
    let skill_accuracy = accuracy(skill_score);
    let token_reduction = token_reduction(&pairs);
    let shape_reductions = shape_token_reductions(&pairs);
    let mut gates = vec![
        gate(
            "F1 delta",
            review_accuracy.f1 - skill_accuracy.f1,
            prereg.thresholds.f1_delta,
            Comparison::AtLeast,
        ),
        gate(
            "recall delta",
            review_accuracy.recall - skill_accuracy.recall,
            prereg.thresholds.recall_delta,
            Comparison::AtLeast,
        ),
        gate(
            "precision delta",
            review_accuracy.precision - skill_accuracy.precision,
            prereg.thresholds.precision_delta,
            Comparison::AtLeast,
        ),
        gate(
            "Critical/High recall",
            review_accuracy.critical_high_recall,
            prereg.thresholds.critical_high_recall,
            Comparison::AtLeast,
        ),
        gate(
            "median logical-token reduction",
            token_reduction,
            prereg.thresholds.median_logical_token_reduction,
            Comparison::AtLeast,
        ),
    ];
    for (shape, reduction) in shape_reductions {
        gates.push(gate(
            &format!("shape `{shape}` token regression"),
            reduction,
            -prereg.thresholds.maximum_shape_token_regression,
            Comparison::AtLeast,
        ));
    }
    let passed = gates.iter().all(|gate| gate.passed);
    write_json(
        output,
        &serde_json::json!({
            "schemaVersion": 1,
            "claimEvidence": false,
            "claimEnabled": false,
            "stale": !passed,
            "passed": passed,
            "baselineKey": baseline_key(&review.provenance),
            "reason": if passed {
                "Scheduled public regression guard passed; this is not claim evidence."
            } else {
                "A scheduled regression fell below preregistered material thresholds; any prior claim is stale."
            },
            "gates": gates,
        }),
    )?;
    if !passed {
        bail!(
            "nightly reviewer benchmark materially regressed; see {}",
            output.display()
        );
    }
    Ok(())
}

fn run_gate(
    prereg_path: &Path,
    corpus_path: &Path,
    review_path: &Path,
    skill_path: &Path,
    adjudication_path: &Path,
    map_path: &Path,
) -> Result<ClaimStatus> {
    let prereg: Preregistration = read_json(prereg_path)?;
    let corpus: Corpus = read_json(corpus_path)?;
    let review: Capture = read_json(review_path)?;
    let skill: Capture = read_json(skill_path)?;
    let adjudication: AdjudicationReport = read_json(adjudication_path)?;
    let private_map: PrivateMap = read_json(map_path)?;
    validate_inputs(
        &prereg,
        corpus_path,
        &corpus,
        &review,
        &skill,
        &adjudication,
        &private_map,
    )?;
    let pairs = paired_units(&corpus, &review, &skill, prereg.minimum_repetitions)?;
    let review_score = aggregate_score(&pairs, true);
    let skill_score = aggregate_score(&pairs, false);
    let review_accuracy = accuracy(review_score);
    let skill_accuracy = accuracy(skill_score);
    let delta = Accuracy {
        precision: review_accuracy.precision - skill_accuracy.precision,
        recall: review_accuracy.recall - skill_accuracy.recall,
        f1: review_accuracy.f1 - skill_accuracy.f1,
        critical_high_recall: review_accuracy.critical_high_recall
            - skill_accuracy.critical_high_recall,
    };
    let token_reduction = token_reduction(&pairs);
    let token_reduction_by_shape = shape_token_reductions(&pairs);
    let suggestion_success_delta = suggestion_delta(&review, &skill, &adjudication, &private_map)?;
    let confidence = bootstrap(&pairs, &prereg)?;
    let thresholds = &prereg.thresholds;
    let mut gates = vec![
        gate(
            "F1 delta",
            delta.f1,
            thresholds.f1_delta,
            Comparison::AtLeast,
        ),
        gate(
            "recall delta",
            delta.recall,
            thresholds.recall_delta,
            Comparison::AtLeast,
        ),
        gate(
            "precision delta",
            delta.precision,
            thresholds.precision_delta,
            Comparison::AtLeast,
        ),
        gate(
            "Review Critical/High recall",
            review_accuracy.critical_high_recall,
            thresholds.critical_high_recall,
            Comparison::AtLeast,
        ),
        gate(
            "Critical/High recall delta",
            delta.critical_high_recall,
            thresholds.critical_high_recall_delta,
            Comparison::AtLeast,
        ),
        gate(
            "suggestion success delta",
            suggestion_success_delta,
            thresholds.suggestion_success_delta,
            Comparison::AtLeast,
        ),
        gate(
            "median logical-token reduction",
            token_reduction,
            thresholds.median_logical_token_reduction,
            Comparison::AtLeast,
        ),
        gate(
            "F1 delta confidence lower bound",
            confidence.f1_delta[0],
            prereg.confidence_gates.f1_delta_lower_bound,
            Comparison::Greater,
        ),
        gate(
            "recall delta confidence lower bound",
            confidence.recall_delta[0],
            prereg.confidence_gates.recall_delta_lower_bound,
            Comparison::Greater,
        ),
        gate(
            "token-reduction confidence lower bound",
            confidence.logical_token_reduction[0],
            prereg.confidence_gates.logical_token_reduction_lower_bound,
            Comparison::Greater,
        ),
    ];
    for (shape, reduction) in &token_reduction_by_shape {
        gates.push(gate(
            &format!("shape `{shape}` token regression"),
            *reduction,
            -thresholds.maximum_shape_token_regression,
            Comparison::AtLeast,
        ));
    }
    gates.push(gate(
        "blind semantic agreement",
        adjudication.raw_semantic_agreement,
        thresholds.minimum_semantic_agreement,
        Comparison::AtLeast,
    ));
    gates.push(gate(
        "blind Cohen kappa",
        adjudication.cohen_kappa_semantic,
        thresholds.minimum_cohen_kappa,
        Comparison::AtLeast,
    ));
    let passed = gates.iter().all(|gate| gate.passed);
    let baseline_key = baseline_key(&review.provenance);
    Ok(ClaimStatus {
        schema_version: 1,
        claim_enabled: passed,
        stale: false,
        baseline_key: Some(baseline_key),
        evaluated_at_ms: Some(unix_millis()),
        reason: if passed {
            "Every preregistered accuracy, token, confidence, shape, provenance, and human-evidence gate passed."
        } else {
            "One or more preregistered gates failed; superiority claims are disabled."
        }
        .to_owned(),
        gates,
        report: Some(Report {
            discovery: WorkflowComparison {
                review: review_accuracy,
                skill: skill_accuracy,
                delta,
            },
            delivery: DeliveryComparison {
                review_anchor_validity: ratio(
                    review_score.anchors_valid,
                    review_score.anchors_total,
                ),
                skill_anchor_validity: ratio(
                    skill_score.anchors_valid,
                    skill_score.anchors_total,
                ),
                review_duplicate_findings: human_duplicates(
                    &review,
                    &adjudication,
                    &private_map,
                ),
                skill_duplicate_findings: human_duplicates(
                    &skill,
                    &adjudication,
                    &private_map,
                ),
                review_severity_accuracy: human_severity_accuracy(
                    &review,
                    &adjudication,
                    &private_map,
                ),
                skill_severity_accuracy: human_severity_accuracy(
                    &skill,
                    &adjudication,
                    &private_map,
                ),
            },
            token_reduction,
            token_reduction_by_shape,
            suggestion_success_delta,
            confidence_intervals: confidence,
            evidence_packet_hash: adjudication.packet_hash,
        }),
    })
}

fn validate_inputs(
    prereg: &Preregistration,
    corpus_path: &Path,
    corpus: &Corpus,
    review: &Capture,
    skill: &Capture,
    adjudication: &AdjudicationReport,
    private_map: &PrivateMap,
) -> Result<()> {
    if review.side_effects || skill.side_effects {
        bail!("side-effecting captures invalidate the claim");
    }
    if review.complete != Some(true) || skill.complete != Some(true) {
        bail!("both captures must be explicitly complete");
    }
    if review.model != skill.model
        || review.thinking != skill.thinking
        || review.provenance != skill.provenance
    {
        bail!("capture model, thinking, and complete provenance must match");
    }
    if review.model.as_deref() != Some(prereg.baseline_model.as_str())
        || review.thinking.as_deref() != Some(prereg.baseline_thinking.as_str())
    {
        bail!("provider/model or thinking changed; create a new preregistered baseline");
    }
    let canonical = fs::canonicalize(corpus_path)?;
    let repository = fs::canonicalize(env!("CARGO_MANIFEST_DIR"))?;
    if canonical.starts_with(repository) {
        bail!("superiority gate requires the private holdout outside the repository");
    }
    let corpus_hash = blake3::hash(&fs::read(&canonical)?).to_hex().to_string();
    if corpus_hash != review.provenance.source_corpus_hash {
        bail!("holdout hash does not match capture provenance");
    }
    if !adjudication.blind
        || adjudication.disagreements != adjudication.resolved_disagreements
        || adjudication.candidate_count != adjudication.decisions.len()
        || adjudication.packet_hash != private_map.packet_hash
    {
        bail!("blind adjudication is incomplete or has unresolved disagreements");
    }
    let mapped = private_map
        .candidates
        .iter()
        .map(|candidate| candidate.candidate_id.as_str())
        .collect::<BTreeSet<_>>();
    let decided = adjudication
        .decisions
        .iter()
        .map(|decision| decision.candidate_id.as_str())
        .collect::<BTreeSet<_>>();
    if mapped != decided {
        bail!("blind evidence packet and private workflow map differ");
    }
    validate_matched_evidence(corpus, review, skill, adjudication, private_map)?;
    Ok(())
}

fn validate_matched_evidence(
    corpus: &Corpus,
    review: &Capture,
    skill: &Capture,
    adjudication: &AdjudicationReport,
    private_map: &PrivateMap,
) -> Result<()> {
    let cases = corpus
        .cases
        .iter()
        .map(|case| (case.id.as_str(), case))
        .collect::<HashMap<_, _>>();
    let decisions = adjudication
        .decisions
        .iter()
        .map(|decision| (decision.candidate_id.as_str(), decision))
        .collect::<HashMap<_, _>>();
    let origins = private_map
        .candidates
        .iter()
        .map(|origin| {
            (
                (
                    origin.workflow.as_str(),
                    origin.case_id.as_str(),
                    origin.repetition,
                    origin.finding_index,
                ),
                origin,
            )
        })
        .collect::<HashMap<_, _>>();
    for capture in [review, skill] {
        for run in &capture.runs {
            let case = cases
                .get(run.case_id.as_str())
                .with_context(|| format!("unknown adjudicated case {}", run.case_id))?;
            for (index, finding) in run.findings.iter().enumerate() {
                let matched = case.expected.iter().any(|expected| {
                    expected.category == finding.category
                        && expected.file == finding.file
                        && (expected.line.is_none() || expected.line == finding.line)
                });
                if !matched {
                    continue;
                }
                let origin = origins
                    .get(&(
                        capture.name.as_str(),
                        run.case_id.as_str(),
                        run.repetition,
                        index,
                    ))
                    .with_context(|| {
                        format!(
                            "matched finding lacks blind evidence: {} {} repetition {} finding {}",
                            capture.name, run.case_id, run.repetition, index
                        )
                    })?;
                if !decisions[origin.candidate_id.as_str()].semantically_correct {
                    bail!(
                        "deterministic matched finding {} was rejected by blind adjudication",
                        origin.candidate_id
                    );
                }
            }
        }
    }
    Ok(())
}

fn paired_units(
    corpus: &Corpus,
    review: &Capture,
    skill: &Capture,
    minimum_repetitions: u32,
) -> Result<Vec<Pair>> {
    if corpus.cases.len() < 100 {
        bail!("superiority comparison requires at least 100 held-out cases");
    }
    let cases = corpus
        .cases
        .iter()
        .map(|case| (case.id.as_str(), case))
        .collect::<HashMap<_, _>>();
    let review_runs = runs_by_key(review)?;
    let skill_runs = runs_by_key(skill)?;
    if review_runs.keys().collect::<BTreeSet<_>>() != skill_runs.keys().collect::<BTreeSet<_>>() {
        bail!("captures do not contain identical paired case/repetition IDs");
    }
    let repetitions = review_runs
        .keys()
        .map(|(_, repetition)| *repetition)
        .collect::<BTreeSet<_>>();
    if repetitions.len() < minimum_repetitions as usize
        || repetitions.iter().copied().max().unwrap_or(0) < minimum_repetitions
    {
        bail!("fewer than {minimum_repetitions} paired repetitions are present");
    }
    for repetition in 1..=minimum_repetitions {
        for case in &corpus.cases {
            if !review_runs.contains_key(&(case.id.clone(), repetition)) {
                bail!(
                    "paired comparison is missing case `{}` repetition {repetition}",
                    case.id
                );
            }
        }
    }
    let mut pairs = Vec::new();
    for (key, review_run) in review_runs {
        let skill_run = skill_runs[&key];
        let case = cases
            .get(key.0.as_str())
            .with_context(|| format!("unknown case {}", key.0))?;
        pairs.push(Pair {
            shape: case.shape.clone(),
            review: score_run(case, review_run),
            skill: score_run(case, skill_run),
            review_tokens: review_run
                .tokens
                .logical()
                .context("Review token telemetry is incomplete")?,
            skill_tokens: skill_run
                .tokens
                .logical()
                .context("skill token telemetry is incomplete")?,
        });
    }
    Ok(pairs)
}

fn runs_by_key(capture: &Capture) -> Result<BTreeMap<(String, u32), &Run>> {
    let mut runs = BTreeMap::new();
    for run in &capture.runs {
        if runs
            .insert((run.case_id.clone(), run.repetition), run)
            .is_some()
        {
            bail!("capture {} has duplicate paired IDs", capture.name);
        }
    }
    Ok(runs)
}

fn score_run(case: &Case, run: &Run) -> Score {
    let mut score = Score::default();
    let mut matched = vec![false; case.expected.len()];
    let mut finding_keys = BTreeSet::new();
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
        if !finding_keys.insert((&finding.category, &finding.file, finding.line)) {
            score.duplicates += 1;
        }
        if let Some(index) = case
            .expected
            .iter()
            .enumerate()
            .position(|(index, expected)| {
                !matched[index]
                    && expected.category == finding.category
                    && expected.file == finding.file
                    && (expected.line.is_none() || expected.line == finding.line)
            })
        {
            matched[index] = true;
            score.tp += 1;
        } else {
            score.fp += 1;
        }
    }
    for (expected, matched) in case.expected.iter().zip(matched) {
        score.fn_ += usize::from(!matched);
        if matches!(expected.severity.as_deref(), Some("Critical" | "High")) {
            score.high_total += 1;
            score.high_tp += usize::from(matched);
        }
    }
    score
}

fn aggregate_score(pairs: &[Pair], review: bool) -> Score {
    let mut score = Score::default();
    for pair in pairs {
        score.add(if review { pair.review } else { pair.skill });
    }
    score
}

fn accuracy(score: Score) -> Accuracy {
    Accuracy {
        precision: score.precision(),
        recall: score.recall(),
        f1: score.f1(),
        critical_high_recall: score.high_recall(),
    }
}

fn token_reduction(pairs: &[Pair]) -> f64 {
    let review = median(
        &mut pairs
            .iter()
            .map(|pair| pair.review_tokens)
            .collect::<Vec<_>>(),
    );
    let skill = median(
        &mut pairs
            .iter()
            .map(|pair| pair.skill_tokens)
            .collect::<Vec<_>>(),
    );
    if skill == 0.0 {
        0.0
    } else {
        1.0 - review / skill
    }
}

fn shape_token_reductions(pairs: &[Pair]) -> BTreeMap<String, f64> {
    let mut buckets = BTreeMap::<String, Vec<&Pair>>::new();
    for pair in pairs {
        buckets.entry(pair.shape.clone()).or_default().push(pair);
    }
    buckets
        .into_iter()
        .map(|(shape, pairs)| (shape, token_reduction_refs(&pairs)))
        .collect()
}

fn token_reduction_refs(pairs: &[&Pair]) -> f64 {
    let review = median(
        &mut pairs
            .iter()
            .map(|pair| pair.review_tokens)
            .collect::<Vec<_>>(),
    );
    let skill = median(
        &mut pairs
            .iter()
            .map(|pair| pair.skill_tokens)
            .collect::<Vec<_>>(),
    );
    if skill == 0.0 {
        0.0
    } else {
        1.0 - review / skill
    }
}

fn suggestion_delta(
    review: &Capture,
    skill: &Capture,
    adjudication: &AdjudicationReport,
    private_map: &PrivateMap,
) -> Result<f64> {
    let decisions = adjudication
        .decisions
        .iter()
        .map(|decision| (decision.candidate_id.as_str(), decision))
        .collect::<HashMap<_, _>>();
    let mut totals = HashMap::<&str, (usize, usize)>::new();
    for origin in &private_map.candidates {
        let decision = decisions[origin.candidate_id.as_str()];
        if decision.fix_semantically_correct.is_none() {
            continue;
        }
        let successful = decision.semantically_correct
            && decision.fix_semantically_correct == Some(true)
            && validation_success(decision);
        let entry = totals.entry(origin.workflow.as_str()).or_default();
        entry.0 += usize::from(successful);
        entry.1 += 1;
    }
    let review_rate = totals
        .get(review.name.as_str())
        .map(|(success, total)| ratio(*success, *total))
        .context("human evidence has no Review suggestions")?;
    let skill_rate = totals
        .get(skill.name.as_str())
        .map(|(success, total)| ratio(*success, *total))
        .context("human evidence has no skill suggestions")?;
    Ok(review_rate - skill_rate)
}

fn human_duplicates(
    capture: &Capture,
    adjudication: &AdjudicationReport,
    private_map: &PrivateMap,
) -> usize {
    let decisions = adjudication
        .decisions
        .iter()
        .map(|decision| (decision.candidate_id.as_str(), decision))
        .collect::<HashMap<_, _>>();
    let mut groups = HashMap::<(&str, u32, &str), usize>::new();
    for origin in private_map
        .candidates
        .iter()
        .filter(|origin| origin.workflow == capture.name)
    {
        let Some(group) = decisions[origin.candidate_id.as_str()]
            .duplicate_group
            .as_deref()
        else {
            continue;
        };
        *groups
            .entry((origin.case_id.as_str(), origin.repetition, group))
            .or_default() += 1;
    }
    groups.values().map(|count| count.saturating_sub(1)).sum()
}

fn human_severity_accuracy(
    capture: &Capture,
    adjudication: &AdjudicationReport,
    private_map: &PrivateMap,
) -> f64 {
    let decisions = adjudication
        .decisions
        .iter()
        .map(|decision| (decision.candidate_id.as_str(), decision))
        .collect::<HashMap<_, _>>();
    let relevant = private_map
        .candidates
        .iter()
        .filter(|origin| origin.workflow == capture.name)
        .collect::<Vec<_>>();
    ratio(
        relevant
            .iter()
            .filter(|origin| decisions[origin.candidate_id.as_str()].severity_correct)
            .count(),
        relevant.len(),
    )
}

fn validation_success(decision: &Decision) -> bool {
    decision.application == ValidationOutcome::Pass
        && [
            decision.formatter,
            decision.parser,
            decision.build,
            decision.tests,
        ]
        .into_iter()
        .all(|outcome| {
            !matches!(
                outcome,
                ValidationOutcome::Fail | ValidationOutcome::Unavailable
            )
        })
}

fn bootstrap(pairs: &[Pair], prereg: &Preregistration) -> Result<ConfidenceIntervals> {
    if prereg.bootstrap_resamples < 1000
        || !(0.90..1.0).contains(&prereg.confidence_level)
        || pairs.is_empty()
    {
        bail!("invalid preregistered bootstrap configuration");
    }
    let mut rng = Lcg(prereg.bootstrap_seed);
    let mut f1 = Vec::with_capacity(prereg.bootstrap_resamples);
    let mut recall = Vec::with_capacity(prereg.bootstrap_resamples);
    let mut tokens = Vec::with_capacity(prereg.bootstrap_resamples);
    for _ in 0..prereg.bootstrap_resamples {
        let mut review = Score::default();
        let mut skill = Score::default();
        let mut review_tokens = Vec::with_capacity(pairs.len());
        let mut skill_tokens = Vec::with_capacity(pairs.len());
        for _ in 0..pairs.len() {
            let pair = &pairs[rng.next_index(pairs.len())];
            review.add(pair.review);
            skill.add(pair.skill);
            review_tokens.push(pair.review_tokens);
            skill_tokens.push(pair.skill_tokens);
        }
        f1.push(review.f1() - skill.f1());
        recall.push(review.recall() - skill.recall());
        let skill_median = median(&mut skill_tokens);
        tokens.push(if skill_median == 0.0 {
            0.0
        } else {
            1.0 - median(&mut review_tokens) / skill_median
        });
    }
    Ok(ConfidenceIntervals {
        f1_delta: percentile_interval(&mut f1, prereg.confidence_level),
        recall_delta: percentile_interval(&mut recall, prereg.confidence_level),
        logical_token_reduction: percentile_interval(&mut tokens, prereg.confidence_level),
    })
}

fn percentile_interval(values: &mut [f64], confidence: f64) -> [f64; 2] {
    values.sort_by(f64::total_cmp);
    let tail = (1.0 - confidence) / 2.0;
    let low = (tail * values.len() as f64).floor() as usize;
    let high = ((1.0 - tail) * values.len() as f64).ceil() as usize - 1;
    [values[low], values[high.min(values.len() - 1)]]
}

struct Lcg(u64);

impl Lcg {
    fn next_index(&mut self, length: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 as usize) % length
    }
}

enum Comparison {
    AtLeast,
    Greater,
}

fn gate(name: &str, observed: f64, required: f64, comparison: Comparison) -> Gate {
    let passed = match comparison {
        Comparison::AtLeast => observed >= required,
        Comparison::Greater => observed > required,
    };
    Gate {
        name: name.to_owned(),
        passed,
        observed: format!("{observed:.6}"),
        required: match comparison {
            Comparison::AtLeast => format!(">= {required:.6}"),
            Comparison::Greater => format!("> {required:.6}"),
        },
    }
}

fn baseline_key(provenance: &Provenance) -> String {
    blake3::hash(
        format!(
            "{}|{}|{}|{}|{}|{}|{}",
            provenance.provider_model,
            provenance.thinking,
            provenance.environment_version,
            provenance.source_corpus_hash,
            provenance.skill_hash,
            provenance.workflow_commit,
            provenance.workflow_tree_hash
        )
        .as_bytes(),
    )
    .to_hex()
    .to_string()
}

fn check_status(path: &Path) -> Result<()> {
    let status: ClaimStatusRead = read_json(path)?;
    if status.claim_enabled && (status.stale || status.baseline_key.is_none()) {
        bail!("enabled superiority claim is stale or has no baseline identity");
    }
    println!(
        "reviewer superiority claim: {}",
        if status.claim_enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaimStatusRead {
    claim_enabled: bool,
    stale: bool,
    baseline_key: Option<String>,
}

fn median(values: &mut [u64]) -> f64 {
    values.sort_unstable();
    let middle = values.len() / 2;
    if values.len() % 2 == 0 {
        (values[middle - 1] as f64 + values[middle] as f64) / 2.0
    } else {
        values[middle] as f64
    }
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        1.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    serde_json::from_slice(&fs::read(path).with_context(|| format!("read {}", path.display()))?)
        .with_context(|| format!("parse {}", path.display()))
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    fs::write(path, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("write {}", path.display()))
}

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_is_deterministic_and_paired() {
        let pairs = (0..20)
            .map(|_| Pair {
                shape: "small".into(),
                review: Score {
                    tp: 2,
                    ..Score::default()
                },
                skill: Score {
                    tp: 1,
                    fn_: 1,
                    ..Score::default()
                },
                review_tokens: 50,
                skill_tokens: 100,
            })
            .collect::<Vec<_>>();
        let prereg = Preregistration {
            baseline_model: "model".into(),
            baseline_thinking: "high".into(),
            minimum_repetitions: 10,
            bootstrap_resamples: 1000,
            bootstrap_seed: 7,
            confidence_level: 0.95,
            thresholds: serde_json::from_str(r#"{"f1Delta":0.05,"recallDelta":0.08,"precisionDelta":-0.01,"criticalHighRecall":0.95,"criticalHighRecallDelta":0.0,"suggestionSuccessDelta":0.05,"medianLogicalTokenReduction":0.25,"maximumShapeTokenRegression":0.05,"minimumSemanticAgreement":0.8,"minimumCohenKappa":0.6}"#).unwrap(),
            confidence_gates: serde_json::from_str(r#"{"f1DeltaLowerBound":0.0,"recallDeltaLowerBound":0.0,"logicalTokenReductionLowerBound":0.0}"#).unwrap(),
        };
        let first = bootstrap(&pairs, &prereg).unwrap();
        let second = bootstrap(&pairs, &prereg).unwrap();
        assert_eq!(first.f1_delta, second.f1_delta);
        assert_eq!(first.logical_token_reduction, [0.5, 0.5]);
    }

    #[test]
    fn validation_outcomes_fail_closed_on_unavailable() {
        let decision = Decision {
            candidate_id: "candidate".into(),
            semantically_correct: true,
            severity_correct: true,
            duplicate_group: None,
            fix_semantically_correct: Some(true),
            application: ValidationOutcome::Pass,
            formatter: ValidationOutcome::NotApplicable,
            parser: ValidationOutcome::Unavailable,
            build: ValidationOutcome::NotApplicable,
            tests: ValidationOutcome::NotApplicable,
        };
        assert!(!validation_success(&decision));
    }

    #[test]
    fn baseline_changes_with_model_environment_corpus_skill_or_workflow() {
        let mut provenance = Provenance {
            workflow_commit: "workflow".into(),
            workflow_tree_hash: "tree".into(),
            skill_hash: "skill".into(),
            source_corpus_hash: "corpus".into(),
            review_input_hash: "input".into(),
            provider_model: "provider/model".into(),
            thinking: "high".into(),
            tool_permissions: "read-only".into(),
            timeout_seconds: 240,
            environment_version: "environment".into(),
        };
        let original = baseline_key(&provenance);
        provenance.provider_model.push_str("-new");
        assert_ne!(original, baseline_key(&provenance));
    }
}
