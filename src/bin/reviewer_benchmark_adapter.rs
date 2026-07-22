use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;
use wisetree::config::schema::DashboardConfig;
use wisetree::services::{
    opencode_usage_for_title, review_scan_title, DashboardService, ReviewSeverity, ReviewTokenUsage,
};

const DEFAULT_REPO_SKILL: &str =
    "/Users/victorcorcos/Desktop/repositories/skills/skills/reviewer/SKILL.md";
const DEFAULT_INSTALLED_SKILL: &str = "/Users/victorcorcos/.codex/skills/reviewer/SKILL.md";

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Adapter {
    Review,
    Skill,
}

impl Adapter {
    fn capture_name(self) -> &'static str {
        match self {
            Self::Review => "Review",
            Self::Skill => "reviewer-skill",
        }
    }
}

#[derive(Debug, Parser)]
#[command(about = "Leakage-free live adapter for the reviewer benchmark")]
struct Args {
    #[arg(long, value_enum)]
    adapter: Adapter,
    #[arg(long)]
    corpus: PathBuf,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    model: String,
    #[arg(long)]
    thinking: String,
    #[arg(long)]
    repetitions: u32,
    #[arg(long, default_value_t = 240)]
    timeout_seconds: u64,
    #[arg(long, default_value = DEFAULT_REPO_SKILL)]
    repo_skill: PathBuf,
    #[arg(long, default_value = DEFAULT_INSTALLED_SKILL)]
    installed_skill: PathBuf,
    #[arg(long)]
    read_only: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewInput {
    diff: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    context_files: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct RedactedCase {
    id: String,
    review_input: ReviewInput,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct RedactedCorpus {
    cases: Vec<RedactedCase>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Capture {
    name: String,
    model: Option<String>,
    thinking: Option<String>,
    side_effects: bool,
    complete: bool,
    provenance: Provenance,
    failures: Vec<RunFailure>,
    runs: Vec<Run>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Provenance {
    workflow_commit: String,
    workflow_tree_hash: String,
    repository_skill_path: String,
    installed_skill_path: String,
    skill_hash: String,
    source_corpus_hash: String,
    review_input_hash: String,
    provider_model: String,
    thinking: String,
    tool_permissions: String,
    timeout_seconds: u64,
    environment_version: String,
    started_at_ms: u64,
    completed_at_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RunFailure {
    case_id: String,
    repetition: u32,
    reason: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Run {
    case_id: String,
    repetition: u32,
    findings: Vec<Finding>,
    tokens: ReviewTokenUsage,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
struct Finding {
    category: String,
    severity: String,
    file: String,
    line: Option<u64>,
    title: String,
    suggestion: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    validate_args(&args)?;

    let source_bytes = fs::read(&args.corpus)
        .with_context(|| format!("could not read corpus {}", args.corpus.display()))?;
    let source_hash = blake3_hex(&source_bytes);
    let redacted = redact_corpus(&source_bytes)?;
    validate_redacted_corpus(&redacted)?;
    let redacted_bytes = serde_json::to_vec_pretty(&redacted)?;
    let redacted_hash = blake3_hex(&redacted_bytes);
    let review_input_path = review_input_path(&args.output);
    fs::write(&review_input_path, &redacted_bytes).with_context(|| {
        format!(
            "could not write redacted review input {}",
            review_input_path.display()
        )
    })?;

    let repo_skill = fs::read(&args.repo_skill).with_context(|| {
        format!(
            "canonical reviewer skill is unavailable at {}",
            args.repo_skill.display()
        )
    })?;
    let installed_skill = fs::read(&args.installed_skill).with_context(|| {
        format!(
            "installed reviewer skill is unavailable at {}",
            args.installed_skill.display()
        )
    })?;
    let skill_hash = blake3_hex(&repo_skill);
    if skill_hash != blake3_hex(&installed_skill) {
        bail!(
            "reviewer skill hash mismatch: installed and repository copies must match before benchmarking"
        );
    }
    let skill_text = String::from_utf8(repo_skill).context("reviewer skill is not UTF-8")?;

    let mut capture = Capture {
        name: args.adapter.capture_name().to_owned(),
        model: Some(args.model.clone()),
        thinking: Some(args.thinking.clone()),
        side_effects: false,
        complete: false,
        provenance: Provenance {
            workflow_commit: workflow_commit()?,
            workflow_tree_hash: workflow_tree_hash()?,
            repository_skill_path: args.repo_skill.display().to_string(),
            installed_skill_path: args.installed_skill.display().to_string(),
            skill_hash,
            source_corpus_hash: source_hash.clone(),
            review_input_hash: redacted_hash,
            provider_model: args.model.clone(),
            thinking: args.thinking.clone(),
            tool_permissions: "opencode:plan; filesystem:fixture-read-only; network:no-posting"
                .to_owned(),
            timeout_seconds: args.timeout_seconds,
            environment_version: environment_version().await?,
            started_at_ms: unix_millis(),
            completed_at_ms: None,
        },
        failures: Vec::new(),
        runs: Vec::new(),
    };
    persist_capture(&args.output, &capture)?;

    for repetition in 1..=args.repetitions {
        for case in &redacted.cases {
            let result = run_case(&args, case, &skill_text).await;
            match result {
                Ok(run) => capture.runs.push(run),
                Err(error) => {
                    capture.failures.push(RunFailure {
                        case_id: case.id.clone(),
                        repetition,
                        reason: format!("{error:#}"),
                    });
                    persist_capture(&args.output, &capture)?;
                    bail!(
                        "benchmark adapter stopped at case `{}` repetition {repetition}; completed repetitions remain in {}: {error:#}",
                        case.id,
                        args.output.display()
                    );
                }
            }
            if let Some(last) = capture.runs.last_mut() {
                last.repetition = repetition;
            }
            persist_capture(&args.output, &capture)?;
        }
    }

    ensure_complete(&redacted, args.repetitions, &capture.runs)?;
    let final_source_hash = blake3_hex(&fs::read(&args.corpus)?);
    if final_source_hash != source_hash {
        capture.side_effects = true;
        persist_capture(&args.output, &capture)?;
        bail!("source corpus changed during benchmark execution");
    }
    capture.complete = true;
    capture.provenance.completed_at_ms = Some(unix_millis());
    persist_capture(&args.output, &capture)?;
    Ok(())
}

fn validate_args(args: &Args) -> Result<()> {
    if !args.read_only || env::var("WISETREE_BENCHMARK_READ_ONLY").as_deref() != Ok("1") {
        bail!("live adapters require --read-only and WISETREE_BENCHMARK_READ_ONLY=1");
    }
    if args.repetitions == 0 {
        bail!("repetitions must be positive");
    }
    if args.timeout_seconds == 0 {
        bail!("timeout-seconds must be positive");
    }
    if args.timeout_seconds != 240 {
        bail!("the production Review timeout is 240 seconds per model turn; benchmark parity requires --timeout-seconds 240");
    }
    if args.model.trim().is_empty() || args.thinking.trim().is_empty() {
        bail!("model and thinking must be explicit");
    }
    Ok(())
}

fn redact_corpus(source: &[u8]) -> Result<RedactedCorpus> {
    let value: Value = serde_json::from_slice(source).context("corpus is not valid JSON")?;
    let cases = value
        .get("cases")
        .and_then(Value::as_array)
        .context("corpus has no cases array")?;
    let mut redacted = Vec::with_capacity(cases.len());
    for case in cases {
        let id = case
            .get("id")
            .and_then(Value::as_str)
            .context("corpus case has no string id")?;
        let diff = case
            .get("reviewInput")
            .and_then(|input| input.get("diff"))
            .and_then(Value::as_str)
            .with_context(|| format!("corpus case `{id}` has no reviewInput.diff"))?;
        let context_files = case
            .get("reviewInput")
            .and_then(|input| input.get("contextFiles"))
            .map(|files| serde_json::from_value(files.clone()))
            .transpose()
            .with_context(|| format!("corpus case `{id}` has invalid contextFiles"))?
            .unwrap_or_default();
        redacted.push(RedactedCase {
            id: id.to_owned(),
            review_input: ReviewInput {
                diff: diff.to_owned(),
                context_files,
            },
        });
    }
    Ok(RedactedCorpus { cases: redacted })
}

fn validate_redacted_corpus(corpus: &RedactedCorpus) -> Result<()> {
    let mut ids = BTreeSet::new();
    for case in &corpus.cases {
        if !ids.insert(case.id.as_str()) {
            bail!("duplicate corpus case `{}`", case.id);
        }
        if case.review_input.diff.trim().is_empty() {
            bail!("corpus case `{}` has an empty diff", case.id);
        }
    }
    if corpus.cases.is_empty() {
        bail!("corpus has no cases");
    }
    Ok(())
}

async fn run_case(args: &Args, case: &RedactedCase, skill: &str) -> Result<Run> {
    let fixture = Fixture::new(
        &case.id,
        &case.review_input.diff,
        &case.review_input.context_files,
    )?;
    let before = hash_tree(fixture.path())?;
    fixture.make_read_only()?;
    let result = match args.adapter {
        Adapter::Review => run_review_pipeline(args, case, fixture.path()).await,
        Adapter::Skill => run_skill_pipeline(args, case, skill, fixture.path()).await,
    };
    fixture.make_writable()?;
    let after = hash_tree(fixture.path())?;
    if before != after {
        bail!("read-only fixture snapshot changed during model execution");
    }
    let (findings, tokens) = result?;
    Ok(Run {
        case_id: case.id.clone(),
        repetition: 0,
        findings,
        tokens,
    })
}

async fn run_review_pipeline(
    args: &Args,
    case: &RedactedCase,
    fixture: &Path,
) -> Result<(Vec<Finding>, ReviewTokenUsage)> {
    let mut config = DashboardConfig::default();
    // Controlled benchmark mode pins every production Review profile to the
    // same model/effort as the canonical skill so the comparison isolates
    // orchestration rather than model mix.
    for profile in [
        &mut config.ai.review.strong,
        &mut config.ai.review.balanced,
        &mut config.ai.review.utility,
    ] {
        profile.model = args.model.clone();
        profile.thinking = args.thinking.clone();
    }
    let service = DashboardService::new(fixture.to_path_buf(), config);
    let outcome = service
        .benchmark_review_diff(
            fixture
                .to_str()
                .context("benchmark fixture path is not UTF-8")?,
            &case.review_input.diff,
        )
        .await
        .context("production Review pipeline failed")?;
    let findings = outcome
        .findings
        .into_iter()
        .map(|finding| Finding {
            category: finding.category,
            severity: match finding.severity {
                ReviewSeverity::Critical => "Critical",
                ReviewSeverity::High => "High",
                ReviewSeverity::Medium => "Medium",
                ReviewSeverity::Low => "Low",
            }
            .to_owned(),
            file: finding.file,
            line: finding.line,
            title: finding.title,
            suggestion: finding.suggestion,
        })
        .collect();
    if outcome.usage.logical_total().is_none() {
        bail!("production Review pipeline completed with unavailable token telemetry");
    }
    Ok((findings, outcome.usage))
}

async fn run_skill_pipeline(
    args: &Args,
    case: &RedactedCase,
    skill: &str,
    fixture: &Path,
) -> Result<(Vec<Finding>, ReviewTokenUsage)> {
    let prompt = skill_prompt(case, skill);
    let title = review_scan_title();
    let mut command = Command::new("opencode");
    command
        .kill_on_drop(true)
        .arg("run")
        .arg("--model")
        .arg(&args.model)
        .arg("--variant")
        .arg(&args.thinking)
        .arg("--agent")
        .arg("plan")
        .arg("--title")
        .arg(&title)
        .arg("--dir")
        .arg(fixture)
        .arg(prompt)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = tokio::time::timeout(Duration::from_secs(args.timeout_seconds), command.output())
        .await
        .with_context(|| format!("model timed out after {} seconds", args.timeout_seconds))?
        .context("could not start opencode; install it and configure model credentials")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("opencode failed: {}", stderr.trim());
    }
    let text = opencode_text(&output.stdout)?;
    let findings = parse_findings(&text)?;
    let tokens = wait_for_usage(title)
        .await
        .context("opencode completed but session token telemetry is unavailable")?;
    Ok((findings, tokens))
}

fn skill_prompt(case: &RedactedCase, skill: &str) -> String {
    let context = render_context_files(&case.review_input.context_files);
    format!(
        "You are executing the canonical reviewer skill copied below. Follow its discovery and self-review method, but this benchmark is strictly read-only: do not modify files, call git or gh, post comments, or use expected labels. Repository evidence consists of review_input.diff, materialized new-side changed files in the fixture, and the provided objective context files; you may read those files when the skill calls for it. Finish by returning exactly the WISETREE structured block described after the skill.\n\n<canonical-reviewer-skill>\n{skill}\n</canonical-reviewer-skill>\n\n<review-input case-id=\"{}\">\n{}\n\n<provided-context-files>\n{}\n</provided-context-files>\n</review-input>\n\nOutput exactly one block and no prose:\n===WISETREE-REVIEW-BEGIN===\nNO-FINDINGS\n===WISETREE-REVIEW-END===\n\nOr replace NO-FINDINGS with one or more chunks in this shape:\n---FINDING---\nCATEGORY: <Code Smell | Security | Performance | Test Quality | Convention>\nSEVERITY: <Critical | High | Medium | Low>\nFILE: <exact changed path>\nLINE: <new-side line or empty>\nSTART_LINE: <range start or empty>\nTITLE: <short title>\n---EXPLANATION---\n<problem and concrete fix>\n---SUGGESTION---\n<exact replacement when directly applicable; otherwise omit this marker>\n---END-FINDING---",
        case.id, case.review_input.diff, context
    )
}

fn render_context_files(files: &BTreeMap<String, String>) -> String {
    if files.is_empty() {
        return "none".to_owned();
    }
    files
        .iter()
        .map(|(path, contents)| format!("--- {path} ---\n{contents}"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn opencode_text(bytes: &[u8]) -> Result<String> {
    let raw = String::from_utf8(bytes.to_vec()).context("opencode output is not UTF-8")?;
    let mut text = String::new();
    for line in raw.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if event.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(value) = event
                .get("part")
                .and_then(|part| part.get("text"))
                .and_then(Value::as_str)
            {
                text.push_str(value);
            }
        }
    }
    if text.trim().is_empty() {
        text = raw;
    }
    Ok(text)
}

fn parse_findings(output: &str) -> Result<Vec<Finding>> {
    let body = output
        .split_once("===WISETREE-REVIEW-BEGIN===")
        .and_then(|(_, rest)| rest.split_once("===WISETREE-REVIEW-END==="))
        .map(|(body, _)| body.trim())
        .context("model output did not contain one complete WISETREE review block")?;
    if body == "NO-FINDINGS" {
        return Ok(Vec::new());
    }
    let mut findings = Vec::new();
    for chunk in body.split("---FINDING---").skip(1) {
        let Some((chunk, _)) = chunk.split_once("---END-FINDING---") else {
            bail!("model output contains an unterminated finding");
        };
        let (headers, remainder) = chunk
            .split_once("---EXPLANATION---")
            .context("finding has no explanation marker")?;
        let headers = parse_headers(headers);
        let category = required_header(&headers, "CATEGORY")?;
        let severity = required_header(&headers, "SEVERITY")?;
        let file = required_header(&headers, "FILE")?;
        let title = required_header(&headers, "TITLE")?;
        let line = headers
            .get("LINE")
            .filter(|line| !line.is_empty())
            .map(|line| line.parse::<u64>())
            .transpose()
            .context("finding LINE is not numeric")?;
        let suggestion = remainder
            .split_once("---SUGGESTION---")
            .map(|(_, suggestion)| suggestion.trim().to_owned());
        findings.push(Finding {
            category,
            severity,
            file,
            line,
            title,
            suggestion,
        });
    }
    if findings.is_empty() {
        bail!("review block contains neither NO-FINDINGS nor findings");
    }
    Ok(findings)
}

fn parse_headers(headers: &str) -> BTreeMap<String, String> {
    headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .collect()
}

fn required_header(headers: &BTreeMap<String, String>, key: &str) -> Result<String> {
    headers
        .get(key)
        .filter(|value| !value.is_empty())
        .cloned()
        .with_context(|| format!("finding has no {key}"))
}

async fn wait_for_usage(title: String) -> Option<ReviewTokenUsage> {
    for _ in 0..20 {
        if let Some(usage) = opencode_usage_for_title(title.clone()).await {
            if usage.logical_total().is_some() {
                return Some(usage);
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

fn ensure_complete(corpus: &RedactedCorpus, repetitions: u32, runs: &[Run]) -> Result<()> {
    let expected = corpus.cases.len() * repetitions as usize;
    if runs.len() != expected {
        bail!(
            "incomplete capture: expected {expected} runs, recorded {}",
            runs.len()
        );
    }
    let actual = runs
        .iter()
        .map(|run| (run.case_id.as_str(), run.repetition))
        .collect::<BTreeSet<_>>();
    for repetition in 1..=repetitions {
        for case in &corpus.cases {
            if !actual.contains(&(case.id.as_str(), repetition)) {
                bail!(
                    "incomplete capture: missing case `{}` repetition {repetition}",
                    case.id
                );
            }
        }
    }
    Ok(())
}

fn persist_capture(path: &Path, capture: &Capture) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let temp = path.with_extension("json.tmp");
    fs::write(&temp, serde_json::to_vec_pretty(capture)?)?;
    fs::rename(&temp, path)?;
    Ok(())
}

fn review_input_path(output: &Path) -> PathBuf {
    let stem = output
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("capture");
    output.with_file_name(format!("{stem}.review-input.json"))
}

struct Fixture {
    directory: PathBuf,
}

impl Fixture {
    fn new(case_id: &str, diff: &str, context_files: &BTreeMap<String, String>) -> Result<Self> {
        let safe_id = case_id
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
            .collect::<String>();
        let directory = env::temp_dir().join(format!(
            "wisetree-review-benchmark-{safe_id}-{}-{}",
            std::process::id(),
            unix_millis()
        ));
        fs::create_dir(&directory)?;
        fs::write(directory.join("review_input.diff"), diff)?;
        for (path, contents) in context_files {
            validate_fixture_path(path)?;
            let target = directory.join(path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(target, contents)?;
        }
        for (path, contents) in reconstruct_current_files(diff)? {
            validate_fixture_path(&path)?;
            let target = directory.join(path);
            if target.exists() {
                continue;
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(target, contents)?;
        }
        Ok(Self { directory })
    }

    fn path(&self) -> &Path {
        &self.directory
    }

    fn make_read_only(&self) -> Result<()> {
        set_tree_read_only(&self.directory, true)
    }

    fn make_writable(&self) -> Result<()> {
        set_tree_read_only(&self.directory, false)
    }
}

fn validate_fixture_path(path: &str) -> Result<()> {
    let path = Path::new(path);
    if path.is_absolute()
        || path.components().any(|component| {
            !matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
    {
        bail!("path escapes benchmark fixture: {}", path.display());
    }
    Ok(())
}

fn reconstruct_current_files(diff: &str) -> Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::<String, Vec<String>>::new();
    let mut current = None::<String>;
    let mut new_line = 0usize;
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            current = Some(path.to_owned());
            files.entry(path.to_owned()).or_default();
            continue;
        }
        if line == "+++ /dev/null" {
            current = None;
            continue;
        }
        if line.starts_with("@@") {
            let start = line
                .split_once(" +")
                .and_then(|(_, rest)| rest.split([',', ' ']).next())
                .and_then(|value| value.parse::<usize>().ok())
                .context("benchmark diff has an invalid hunk header")?;
            new_line = start;
            if let Some(path) = &current {
                let content = files.entry(path.clone()).or_default();
                while content.len() < start.saturating_sub(1) {
                    content.push(String::new());
                }
            }
            continue;
        }
        let Some(path) = &current else {
            continue;
        };
        if line.starts_with("diff --git") || line.starts_with("--- ") {
            continue;
        }
        let Some(prefix) = line.chars().next() else {
            continue;
        };
        if prefix == '-' || prefix == '\\' {
            continue;
        }
        if prefix == '+' || prefix == ' ' {
            let content = files.entry(path.clone()).or_default();
            while content.len() < new_line.saturating_sub(1) {
                content.push(String::new());
            }
            content.push(line[1..].to_owned());
            new_line += 1;
        }
    }
    Ok(files
        .into_iter()
        .map(|(path, lines)| (path, format!("{}\n", lines.join("\n"))))
        .collect())
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.make_writable();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn set_read_only(path: &Path, read_only: bool) -> Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(read_only);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

fn set_tree_read_only(root: &Path, read_only: bool) -> Result<()> {
    let mut entries = walkdir::WalkDir::new(root)
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    if read_only {
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.depth()));
    } else {
        entries.sort_by_key(|entry| entry.depth());
    }
    for entry in entries {
        set_read_only(entry.path(), read_only)?;
    }
    Ok(())
}

fn hash_tree(root: &Path) -> Result<String> {
    let mut files = walkdir::WalkDir::new(root)
        .into_iter()
        .collect::<std::result::Result<Vec<_>, _>>()?;
    files.sort_by(|left, right| left.path().cmp(right.path()));
    let mut hasher = blake3::Hasher::new();
    for entry in files {
        let relative = entry.path().strip_prefix(root)?;
        hasher.update(relative.to_string_lossy().as_bytes());
        if entry.file_type().is_file() {
            hasher.update(&fs::read(entry.path())?);
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn workflow_commit() -> Result<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(env!("CARGO_MANIFEST_DIR"))
        .args(["rev-parse", "HEAD"])
        .output()
        .context("could not read benchmark workflow commit")?;
    if !output.status.success() {
        bail!("could not read benchmark workflow commit");
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn workflow_tree_hash() -> Result<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut paths = vec![root.join("Cargo.toml"), root.join("Cargo.lock")];
    for directory in ["src", "prompts", "benchmarks/reviewer"] {
        for entry in walkdir::WalkDir::new(root.join(directory)) {
            let entry = entry?;
            if !entry.file_type().is_file()
                || entry.path().components().any(|component| {
                    component.as_os_str() == "captured" || component.as_os_str() == "target"
                })
            {
                continue;
            }
            paths.push(entry.path().to_path_buf());
        }
    }
    paths.sort();
    paths.dedup();
    let mut hasher = blake3::Hasher::new();
    for path in paths {
        let relative = path.strip_prefix(root)?;
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update(&fs::read(&path)?);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

async fn environment_version() -> Result<String> {
    let output = Command::new("opencode")
        .arg("--version")
        .output()
        .await
        .context("opencode is unavailable")?;
    if !output.status.success() {
        bail!("opencode --version failed");
    }
    Ok(format!(
        "wisetree/{}; opencode/{}; {}/{}",
        env!("CARGO_PKG_VERSION"),
        String::from_utf8_lossy(&output.stdout).trim(),
        env::consts::OS,
        env::consts::ARCH
    ))
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
    fn redaction_whitelists_only_case_id_and_diff() {
        let source = br#"{"cases":[{"id":"a","tags":["security"],"reviewInput":{"diff":"secret-free diff","hidden":"answer"},"validAnchors":{"x":[1]},"expected":[{"title":"leaked label"}],"notes":"severity High"}]}"#;
        let redacted = redact_corpus(source).unwrap();
        let output = serde_json::to_string(&redacted).unwrap();
        assert!(output.contains("secret-free diff"));
        for forbidden in [
            "security",
            "expected",
            "validAnchors",
            "leaked label",
            "severity",
        ] {
            assert!(!output.contains(forbidden), "leaked `{forbidden}`");
        }
    }

    #[test]
    fn parser_accepts_structured_findings_and_empty_suggestions() {
        let output = "===WISETREE-REVIEW-BEGIN===\n---FINDING---\nCATEGORY: Security\nSEVERITY: High\nFILE: src/a.rs\nLINE: 4\nSTART_LINE:\nTITLE: Remove script\n---EXPLANATION---\nunsafe\n---SUGGESTION---\n\n---END-FINDING---\n===WISETREE-REVIEW-END===";
        assert_eq!(
            parse_findings(output).unwrap(),
            vec![Finding {
                category: "Security".into(),
                severity: "High".into(),
                file: "src/a.rs".into(),
                line: Some(4),
                title: "Remove script".into(),
                suggestion: Some(String::new()),
            }]
        );
    }

    #[test]
    fn completeness_requires_every_case_and_repetition() {
        let corpus = RedactedCorpus {
            cases: vec![RedactedCase {
                id: "a".into(),
                review_input: ReviewInput {
                    diff: "x".into(),
                    context_files: BTreeMap::new(),
                },
            }],
        };
        let run = Run {
            case_id: "a".into(),
            repetition: 1,
            findings: Vec::new(),
            tokens: ReviewTokenUsage::default(),
        };
        assert!(ensure_complete(&corpus, 1, &[run]).is_ok());
        assert!(ensure_complete(&corpus, 2, &[]).is_err());
    }

    #[test]
    fn tree_hash_detects_fixture_mutation() {
        let fixture = Fixture::new("hash", "before", &BTreeMap::new()).unwrap();
        let before = hash_tree(fixture.path()).unwrap();
        fs::write(fixture.path().join("review_input.diff"), "after").unwrap();
        assert_ne!(before, hash_tree(fixture.path()).unwrap());
    }

    #[test]
    fn fixture_is_made_read_only_and_restored() {
        let fixture = Fixture::new("permissions", "diff", &BTreeMap::new()).unwrap();
        fixture.make_read_only().unwrap();
        assert!(fs::metadata(fixture.path())
            .unwrap()
            .permissions()
            .readonly());
        assert!(fs::metadata(fixture.path().join("review_input.diff"))
            .unwrap()
            .permissions()
            .readonly());
        fixture.make_writable().unwrap();
        assert!(!fs::metadata(fixture.path())
            .unwrap()
            .permissions()
            .readonly());
    }

    #[test]
    fn missing_opencode_is_reported_as_unavailable() {
        let error = std::process::Command::new("definitely-not-an-opencode-binary")
            .arg("--version")
            .output()
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn reconstructed_fixture_contains_only_new_side_content_at_real_lines() {
        let files = reconstruct_current_files(
            "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -3,2 +3,2 @@\n-old();\n+new();\n tail();\n",
        )
        .unwrap();
        assert_eq!(files["src/a.rs"], "\n\nnew();\ntail();\n");
        assert!(!files["src/a.rs"].contains("old"));
    }

    #[test]
    fn fixture_paths_reject_absolute_and_parent_traversal() {
        assert!(validate_fixture_path("src/lib.rs").is_ok());
        assert!(validate_fixture_path("../labels.json").is_err());
        assert!(validate_fixture_path("/tmp/labels.json").is_err());
    }
}
