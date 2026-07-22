use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize)]
struct Corpus {
    cases: Vec<CorpusCase>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CorpusCase {
    id: String,
    review_input: Value,
}

#[derive(Debug, Deserialize)]
struct Capture {
    name: String,
    runs: Vec<CapturedRun>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CapturedRun {
    case_id: String,
    repetition: u32,
    findings: Vec<Value>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlindPacket {
    schema_version: u32,
    packet_hash: String,
    cases: Vec<BlindCase>,
    candidates: Vec<BlindCandidate>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlindCase {
    id: String,
    review_input: Value,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlindCandidate {
    candidate_id: String,
    case_id: String,
    repetition: u32,
    finding: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PrivateMap {
    packet_hash: String,
    candidates: Vec<CandidateOrigin>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CandidateOrigin {
    candidate_id: String,
    workflow: String,
    case_id: String,
    repetition: u32,
    finding_index: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
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
    notes: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum ValidationOutcome {
    Pass,
    Fail,
    NotApplicable,
    Unavailable,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DecisionSet {
    adjudicator_id: String,
    packet_hash: String,
    blind: bool,
    decisions: Vec<Decision>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AdjudicationReport {
    schema_version: u32,
    packet_hash: String,
    adjudicators: [String; 2],
    blind: bool,
    candidate_count: usize,
    raw_semantic_agreement: f64,
    cohen_kappa_semantic: f64,
    disagreements: usize,
    resolved_disagreements: usize,
    decisions: Vec<Decision>,
}

fn main() -> Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    match args.first().map(String::as_str) {
        Some("packet") if args.len() == 5 => packet(
            Path::new(&args[1]),
            [Path::new(&args[2]), Path::new(&args[3])],
            Path::new(&args[4]),
        ),
        Some("resolve") if args.len() == 6 => resolve(
            Path::new(&args[1]),
            Path::new(&args[2]),
            Path::new(&args[3]),
            Path::new(&args[4]),
            Path::new(&args[5]),
        ),
        _ => bail!(
            "usage: reviewer_adjudication packet <corpus> <capture-a> <capture-b> <packet> | reviewer_adjudication resolve <packet> <adjudicator-a> <adjudicator-b> <resolution> <report>"
        ),
    }
}

fn packet(corpus_path: &Path, captures: [&Path; 2], output: &Path) -> Result<()> {
    let corpus: Corpus = read_json(corpus_path)?;
    let mut candidates = Vec::new();
    let mut origins = Vec::new();
    for capture_path in captures {
        let capture: Capture = read_json(capture_path)?;
        for run in capture.runs {
            for (index, finding) in run.findings.into_iter().enumerate() {
                let identity = serde_json::to_vec(&serde_json::json!({
                    "workflow": capture.name,
                    "caseId": run.case_id,
                    "repetition": run.repetition,
                    "finding": finding,
                    "index": index,
                }))?;
                let candidate_id = format!("candidate-{}", &blake3_hex(&identity)[..16]);
                candidates.push(BlindCandidate {
                    candidate_id: candidate_id.clone(),
                    case_id: run.case_id.clone(),
                    repetition: run.repetition,
                    finding,
                });
                origins.push(CandidateOrigin {
                    candidate_id,
                    workflow: capture.name.clone(),
                    case_id: run.case_id.clone(),
                    repetition: run.repetition,
                    finding_index: index,
                });
            }
        }
    }
    candidates.sort_by(|left, right| left.candidate_id.cmp(&right.candidate_id));
    origins.sort_by(|left, right| left.candidate_id.cmp(&right.candidate_id));
    let cases = corpus
        .cases
        .into_iter()
        .map(|case| BlindCase {
            id: case.id,
            review_input: case.review_input,
        })
        .collect();
    let payload = serde_json::to_vec(&(&cases, &candidates))?;
    let packet_hash = blake3_hex(&payload);
    let packet = BlindPacket {
        schema_version: 1,
        packet_hash: packet_hash.clone(),
        cases,
        candidates,
    };
    write_json(output, &packet)?;
    write_json(
        &private_map_path(output),
        &PrivateMap {
            packet_hash,
            candidates: origins,
        },
    )?;
    Ok(())
}

fn resolve(
    packet_path: &Path,
    first_path: &Path,
    second_path: &Path,
    resolution_path: &Path,
    output: &Path,
) -> Result<()> {
    let packet: BlindPacket = read_json(packet_path)?;
    let first: DecisionSet = read_json(first_path)?;
    let second: DecisionSet = read_json(second_path)?;
    let resolution: DecisionSet = read_json(resolution_path)?;
    if first.adjudicator_id == second.adjudicator_id {
        bail!("two distinct blind adjudicators are required");
    }
    for set in [&first, &second, &resolution] {
        if set.packet_hash != packet.packet_hash {
            bail!("decision packet hash does not match the blind packet");
        }
    }
    if !first.blind || !second.blind {
        bail!("both primary adjudicators must attest blind=true");
    }
    let candidate_ids = packet
        .candidates
        .iter()
        .map(|candidate| candidate.candidate_id.as_str())
        .collect::<BTreeSet<_>>();
    let first = decision_map(&first, &candidate_ids, true)?;
    let second = decision_map(&second, &candidate_ids, true)?;
    let resolution = decision_map(&resolution, &candidate_ids, false)?;
    let mut final_decisions = Vec::new();
    let mut disagreements = 0_usize;
    for candidate_id in candidate_ids {
        let left = *first
            .get(candidate_id)
            .with_context(|| format!("missing first decision for {candidate_id}"))?;
        let right = *second
            .get(candidate_id)
            .with_context(|| format!("missing second decision for {candidate_id}"))?;
        if left == right {
            final_decisions.push(left.clone());
        } else {
            disagreements += 1;
            final_decisions.push(
                resolution
                    .get(candidate_id)
                    .with_context(|| format!("unresolved disagreement for {candidate_id}"))?
                    .to_owned()
                    .clone(),
            );
        }
    }
    final_decisions.sort_by(|left, right| left.candidate_id.cmp(&right.candidate_id));
    let total = final_decisions.len();
    let semantic_agreements = first
        .iter()
        .filter(|(id, decision)| decision.semantically_correct == second[*id].semantically_correct)
        .count();
    let raw = ratio(semantic_agreements, total);
    let kappa = cohen_kappa(&first, &second);
    write_json(
        output,
        &AdjudicationReport {
            schema_version: 1,
            packet_hash: packet.packet_hash,
            adjudicators: [
                read_json::<DecisionSet>(first_path)?.adjudicator_id,
                read_json::<DecisionSet>(second_path)?.adjudicator_id,
            ],
            blind: true,
            candidate_count: total,
            raw_semantic_agreement: raw,
            cohen_kappa_semantic: kappa,
            disagreements,
            resolved_disagreements: disagreements,
            decisions: final_decisions,
        },
    )?;
    Ok(())
}

fn decision_map<'a>(
    set: &'a DecisionSet,
    expected: &BTreeSet<&str>,
    require_complete: bool,
) -> Result<BTreeMap<&'a str, &'a Decision>> {
    let mut decisions = BTreeMap::new();
    for decision in &set.decisions {
        if !expected.contains(decision.candidate_id.as_str()) {
            bail!("unknown candidate {}", decision.candidate_id);
        }
        if decisions
            .insert(decision.candidate_id.as_str(), decision)
            .is_some()
        {
            bail!("duplicate decision for {}", decision.candidate_id);
        }
    }
    if require_complete && decisions.len() != expected.len() {
        bail!(
            "adjudicator {} has incomplete decisions",
            set.adjudicator_id
        );
    }
    Ok(decisions)
}

fn cohen_kappa(first: &BTreeMap<&str, &Decision>, second: &BTreeMap<&str, &Decision>) -> f64 {
    let total = first.len();
    if total == 0 {
        return 1.0;
    }
    let observed = ratio(
        first
            .iter()
            .filter(|(id, value)| value.semantically_correct == second[*id].semantically_correct)
            .count(),
        total,
    );
    let first_positive = ratio(
        first
            .values()
            .filter(|decision| decision.semantically_correct)
            .count(),
        total,
    );
    let second_positive = ratio(
        second
            .values()
            .filter(|decision| decision.semantically_correct)
            .count(),
        total,
    );
    let expected =
        first_positive * second_positive + (1.0 - first_positive) * (1.0 - second_positive);
    if (1.0 - expected).abs() < f64::EPSILON {
        1.0
    } else {
        (observed - expected) / (1.0 - expected)
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

fn private_map_path(packet: &Path) -> PathBuf {
    packet.with_extension("map.json")
}

fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        1.0
    } else {
        numerator as f64 / denominator as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decision(id: &str, correct: bool) -> Decision {
        Decision {
            candidate_id: id.into(),
            semantically_correct: correct,
            severity_correct: true,
            duplicate_group: None,
            fix_semantically_correct: None,
            application: ValidationOutcome::NotApplicable,
            formatter: ValidationOutcome::NotApplicable,
            parser: ValidationOutcome::NotApplicable,
            build: ValidationOutcome::NotApplicable,
            tests: ValidationOutcome::NotApplicable,
            notes: String::new(),
        }
    }

    #[test]
    fn kappa_is_one_for_identical_blind_decisions() {
        let first_decision = decision("candidate", true);
        let second_decision = decision("candidate", true);
        let first = BTreeMap::from([("candidate", &first_decision)]);
        let second = BTreeMap::from([("candidate", &second_decision)]);
        assert_eq!(cohen_kappa(&first, &second), 1.0);
    }

    #[test]
    fn blind_packet_schema_has_no_workflow_or_ground_truth() {
        let packet = BlindPacket {
            schema_version: 1,
            packet_hash: "hash".into(),
            cases: vec![BlindCase {
                id: "case".into(),
                review_input: serde_json::json!({"diff":"diff"}),
            }],
            candidates: vec![BlindCandidate {
                candidate_id: "candidate".into(),
                case_id: "case".into(),
                repetition: 1,
                finding: serde_json::json!({"title":"finding"}),
            }],
        };
        let json = serde_json::to_string(&packet).unwrap();
        assert!(!json.contains("workflow"));
        assert!(!json.contains("expected"));
        assert!(!json.contains("severity hint"));
    }
}
