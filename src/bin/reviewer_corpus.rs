use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Corpus {
    schema_version: u32,
    purpose: &'static str,
    cases: Vec<Case>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    id: String,
    tags: Vec<String>,
    shape: String,
    source: Source,
    review_input: ReviewInput,
    valid_anchors: BTreeMap<String, BTreeSet<u64>>,
    expected: Vec<ExpectedFinding>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Source {
    kind: String,
    reference: String,
    independently_documented_by: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReviewInput {
    diff: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    context_files: BTreeMap<String, String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExpectedFinding {
    id: String,
    category: String,
    severity: String,
    file: String,
    line: Option<u64>,
    suggestion: Option<String>,
    equivalence_group: String,
    accepted_fix_patterns: Vec<String>,
    rationale: String,
    minimal_fix: String,
}

struct Template {
    id: &'static str,
    category: &'static str,
    severity: &'static str,
    tags: &'static [&'static str],
    file: &'static str,
    line: Option<u64>,
    diff: &'static str,
    context: &'static [(&'static str, &'static str)],
    rationale: &'static str,
    minimal_fix: &'static str,
    accepted_fix_patterns: &'static [&'static str],
}

fn main() -> Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.len() == 3 && args[0] == "validate-private" {
        return validate_private_corpus(Path::new(&args[1]), &args[2], true);
    }
    if args.len() != 2 || !matches!(args[0].as_str(), "generate" | "check") {
        bail!("usage: reviewer_corpus <generate|check> <corpus.public.json> | reviewer_corpus validate-private <external-corpus.json> <blake3-hash>");
    }
    let corpus = build_corpus()?;
    validate(&corpus)?;
    let bytes = serde_json::to_vec_pretty(&corpus)?;
    match args[0].as_str() {
        "generate" => {
            fs::write(&args[1], bytes).with_context(|| format!("could not write {}", args[1]))?
        }
        "check" => {
            let checked =
                fs::read(&args[1]).with_context(|| format!("could not read {}", args[1]))?;
            if checked != bytes {
                bail!("{} is stale; regenerate it with reviewer_corpus", args[1]);
            }
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn build_corpus() -> Result<Corpus> {
    let mut cases = Vec::new();
    let shapes = [
        "small",
        "medium",
        "large",
        "test-heavy",
        "split-mode",
        "finding-heavy",
    ];
    for template in templates() {
        for shape in shapes {
            cases.push(controlled_case(&template, shape));
        }
    }
    for (commit, category, severity) in historical_sources() {
        cases.push(historical_case(commit, category, severity)?);
    }
    Ok(Corpus {
        schema_version: 2,
        purpose: "public evaluator mechanics and development; never superiority evidence",
        cases,
    })
}

fn controlled_case(template: &Template, shape: &str) -> Case {
    let mut diff = template.diff.to_owned();
    diff.push_str(&shape_evidence(template.id, shape));
    let anchors = parse_valid_anchors(&diff);
    let mut expected = if template.category.is_empty() {
        Vec::new()
    } else {
        vec![ExpectedFinding {
            id: format!("{}-defect", template.id),
            category: template.category.to_owned(),
            severity: template.severity.to_owned(),
            file: template.file.to_owned(),
            line: template.line,
            suggestion: None,
            equivalence_group: format!("{}-behavior", template.id),
            accepted_fix_patterns: template
                .accepted_fix_patterns
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            rationale: template.rationale.to_owned(),
            minimal_fix: template.minimal_fix.to_owned(),
        }]
    };
    if shape == "finding-heavy" && !template.category.is_empty() {
        expected.extend([
            ExpectedFinding {
                id: format!("{}-secondary-command-injection", template.id),
                category: "Security".to_owned(),
                severity: "Critical".to_owned(),
                file: "src/benchmark_extra/command.rs".to_owned(),
                line: Some(7),
                suggestion: None,
                equivalence_group: format!("{}-command-injection", template.id),
                accepted_fix_patterns: vec!["Command::new".to_owned(), ".arg(".to_owned()],
                rationale: "A request parameter is interpolated into a shell command.".to_owned(),
                minimal_fix: "Use direct argv construction without a shell.".to_owned(),
            },
            ExpectedFinding {
                id: format!("{}-secondary-unbounded-collection", template.id),
                category: "Performance".to_owned(),
                severity: "High".to_owned(),
                file: "src/benchmark_extra/export.rs".to_owned(),
                line: Some(11),
                suggestion: None,
                equivalence_group: format!("{}-unbounded-export", template.id),
                accepted_fix_patterns: vec!["take(".to_owned()],
                rationale: "The export eagerly collects every row without a limit.".to_owned(),
                minimal_fix: "Stream rows or enforce a documented bound.".to_owned(),
            },
        ]);
    }
    let mut tags = template
        .tags
        .iter()
        .map(|tag| (*tag).to_owned())
        .collect::<Vec<_>>();
    tags.extend([shape.to_owned(), "controlled-mutation".to_owned()]);
    Case {
        id: format!("{}-{shape}", template.id),
        tags,
        shape: shape.to_owned(),
        source: Source {
            kind: "controlled-mutation".to_owned(),
            reference: format!("benchmarks/reviewer/CORPUS_SPECS.md#{}", template.id),
            independently_documented_by: "mutation author plus blind adjudication packet"
                .to_owned(),
        },
        review_input: ReviewInput {
            diff,
            context_files: template
                .context
                .iter()
                .map(|(path, contents)| ((*path).to_owned(), (*contents).to_owned()))
                .collect(),
        },
        valid_anchors: anchors,
        expected,
    }
}

fn historical_case(commit: &str, category: &str, severity: &str) -> Result<Case> {
    let diff_output = Command::new("git")
        .args([
            "show",
            "-R",
            "--format=",
            "--unified=3",
            "--full-index",
            commit,
        ])
        .output()
        .with_context(|| format!("could not read historical commit {commit}"))?;
    if !diff_output.status.success() {
        bail!("historical source commit {commit} is unavailable");
    }
    let diff = normalize_reverse_diff_prefixes(&String::from_utf8(diff_output.stdout)?);
    let valid_anchors = parse_valid_anchors(&diff);
    let file = valid_anchors
        .keys()
        .next()
        .cloned()
        .with_context(|| format!("historical commit {commit} has no reviewable path"))?;
    let subject = git_subject(commit)?;
    Ok(Case {
        id: format!("historical-regression-{commit}"),
        tags: vec![
            category.to_ascii_lowercase().replace(' ', "-"),
            "historical-regression".to_owned(),
            "revision-heavy".to_owned(),
        ],
        shape: "revision-heavy".to_owned(),
        source: Source {
            kind: "historical-reviewed-defect".to_owned(),
            reference: commit.to_owned(),
            independently_documented_by: format!("reviewed fix commit: {subject}"),
        },
        review_input: ReviewInput {
            diff,
            context_files: BTreeMap::new(),
        },
        valid_anchors,
        expected: vec![ExpectedFinding {
            id: format!("regression-{commit}"),
            category: category.to_owned(),
            severity: severity.to_owned(),
            file,
            line: None,
            suggestion: None,
            equivalence_group: format!("regression-{commit}"),
            accepted_fix_patterns: Vec::new(),
            rationale: format!(
                "Reversing reviewed fix `{subject}` reintroduces its documented defect."
            ),
            minimal_fix: format!("Restore the behavior of reviewed fix commit {commit}."),
        }],
    })
}

fn normalize_reverse_diff_prefixes(diff: &str) -> String {
    diff.lines()
        .map(|line| {
            if let Some(paths) = line.strip_prefix("diff --git b/") {
                if let Some((old, new)) = paths.split_once(" a/") {
                    return format!("diff --git a/{old} b/{new}");
                }
            }
            if let Some(path) = line.strip_prefix("--- b/") {
                return format!("--- a/{path}");
            }
            if let Some(path) = line.strip_prefix("+++ a/") {
                return format!("+++ b/{path}");
            }
            line.to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn git_subject(commit: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["show", "-s", "--format=%s", commit])
        .output()?;
    if !output.status.success() {
        bail!("could not read subject for {commit}");
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn historical_sources() -> &'static [(&'static str, &'static str, &'static str)] {
    &[
        ("d8176fa", "Performance", "High"),
        ("602ca07", "Convention", "Medium"),
        ("76f75be", "Code Smell", "Medium"),
        ("3636ec1", "Code Smell", "High"),
        ("111098d", "Performance", "High"),
        ("b0e43f1", "Security", "High"),
        ("4b50557", "Code Smell", "Medium"),
        ("dcf7800", "Convention", "Medium"),
        ("8187fa2", "Code Smell", "Medium"),
        ("26a2b9e", "Convention", "High"),
    ]
}

fn shape_evidence(id: &str, shape: &str) -> String {
    match shape {
        "small" => String::new(),
        "medium" => format!(
            "diff --git a/src/telemetry/{id}.rs b/src/telemetry/{id}.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/telemetry/{id}.rs\n@@ -0,0 +1,3 @@\n+pub const EVENT: &str = \"{id}\";\n+pub const ENABLED: bool = true;\n+pub const SAMPLE_RATE: u8 = 1;\n"
        ),
        "large" => (0..6)
            .map(|index| format!(
                "diff --git a/src/generated_context/{id}_{index}.rs b/src/generated_context/{id}_{index}.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/generated_context/{id}_{index}.rs\n@@ -0,0 +1,5 @@\n+pub const NAME_{index}: &str = \"{id}\";\n+pub const LIMIT_{index}: usize = {};\n+pub fn enabled_{index}() -> bool {{ true }}\n+pub fn label_{index}() -> &'static str {{ NAME_{index} }}\n+pub fn limit_{index}() -> usize {{ LIMIT_{index} }}\n",
                index + 10
            ))
            .collect(),
        "test-heavy" => format!(
            "diff --git a/tests/{id}_regression.rs b/tests/{id}_regression.rs\nnew file mode 100644\n--- /dev/null\n+++ b/tests/{id}_regression.rs\n@@ -0,0 +1,5 @@\n+#[test]\n+fn preserves_unrelated_baseline() {{\n+    assert_eq!(2 + 2, 4);\n+    assert!(true);\n+}}\n"
        ),
        "split-mode" => format!(
            "diff --git a/crates/api/src/{id}.rs b/crates/api/src/{id}.rs\nnew file mode 100644\n--- /dev/null\n+++ b/crates/api/src/{id}.rs\n@@ -0,0 +1,2 @@\n+pub const API_VERSION: u8 = 1;\n+pub const DOMAIN: &str = \"{id}\";\ndiff --git a/crates/web/tests/{id}.rs b/crates/web/tests/{id}.rs\nnew file mode 100644\n--- /dev/null\n+++ b/crates/web/tests/{id}.rs\n@@ -0,0 +1,2 @@\n+#[test]\n+fn unrelated_contract_stays_stable() {{ assert!(true); }}\n"
        ),
        "finding-heavy" if matches!(id, "clean-documented-constant" | "clean-parameterized-test" | "clean-safe-html") => format!(
            "diff --git a/src/benchmark_extra/{id}_one.rs b/src/benchmark_extra/{id}_one.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/benchmark_extra/{id}_one.rs\n@@ -0,0 +1,2 @@\n+pub const SAFE_LIMIT: usize = 100;\n+pub fn limited(values: &[u8]) -> &[u8] {{ &values[..values.len().min(SAFE_LIMIT)] }}\ndiff --git a/src/benchmark_extra/{id}_two.rs b/src/benchmark_extra/{id}_two.rs\nnew file mode 100644\n--- /dev/null\n+++ b/src/benchmark_extra/{id}_two.rs\n@@ -0,0 +1,2 @@\n+pub fn label(value: &str) -> String {{ html_escape::encode_text(value).to_string() }}\n+pub fn enabled() -> bool {{ true }}\n"
        ),
        "finding-heavy" => "diff --git a/src/benchmark_extra/command.rs b/src/benchmark_extra/command.rs\n--- a/src/benchmark_extra/command.rs\n+++ b/src/benchmark_extra/command.rs\n@@ -6,2 +6,2 @@ pub fn inspect(name: &str) {\n-    Command::new(\"inspect\").arg(name).output()?;\n+    Command::new(\"sh\").arg(\"-c\").arg(format!(\"inspect {name}\")).output()?;\n }\ndiff --git a/src/benchmark_extra/export.rs b/src/benchmark_extra/export.rs\n--- a/src/benchmark_extra/export.rs\n+++ b/src/benchmark_extra/export.rs\n@@ -10,2 +10,2 @@ pub async fn export() {\n-    rows.take(MAX_EXPORT).collect().await\n+    rows.collect().await\n }\n".to_owned(),
        _ => unreachable!(),
    }
}

fn parse_valid_anchors(diff: &str) -> BTreeMap<String, BTreeSet<u64>> {
    let mut anchors = BTreeMap::<String, BTreeSet<u64>>::new();
    let mut file = None::<String>;
    let mut new_line = 0_u64;
    for line in diff.lines() {
        if let Some(path) = line
            .strip_prefix("+++ b/")
            .or_else(|| line.strip_prefix("+++ a/"))
        {
            file = Some(path.to_owned());
            anchors.entry(path.to_owned()).or_default();
        } else if line.starts_with("+++ /dev/null") {
            file = None;
        } else if let Some(header) = line.strip_prefix("@@ -") {
            if let Some((_, new)) = header.split_once(" +") {
                new_line = new
                    .split([',', ' '])
                    .next()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0);
            }
        } else if line.starts_with('+') && !line.starts_with("+++") {
            if let Some(path) = &file {
                anchors.entry(path.clone()).or_default().insert(new_line);
            }
            new_line += 1;
        } else if !line.starts_with('-') && !line.starts_with("diff ") {
            new_line += 1;
        }
    }
    anchors
}

fn validate(corpus: &Corpus) -> Result<()> {
    if corpus.cases.len() < 100 {
        bail!("public corpus must contain at least 100 cases");
    }
    let ids = corpus
        .cases
        .iter()
        .map(|case| case.id.as_str())
        .collect::<BTreeSet<_>>();
    if ids.len() != corpus.cases.len() {
        bail!("public corpus case IDs must be unique");
    }
    let categories = corpus
        .cases
        .iter()
        .flat_map(|case| {
            case.expected
                .iter()
                .map(|finding| finding.category.as_str())
        })
        .collect::<BTreeSet<_>>();
    for required in [
        "Code Smell",
        "Security",
        "Performance",
        "Test Quality",
        "Convention",
    ] {
        if !categories.contains(required) {
            bail!("public corpus is missing category {required}");
        }
    }
    let clean = corpus
        .cases
        .iter()
        .filter(|case| case.expected.is_empty())
        .count();
    if clean < 15 {
        bail!("public corpus needs at least 15 varied clean cases");
    }
    let tags = corpus
        .cases
        .iter()
        .flat_map(|case| case.tags.iter().map(String::as_str))
        .collect::<BTreeSet<_>>();
    for required in [
        "whole-file-deletion",
        "cross-directory",
        "cross-layer",
        "authorization-security",
        "partial-migration",
        "large-file-structure",
        "unconventional-layout",
        "weak-missing-tests",
        "dependency-change",
        "false-positive-trap",
        "finding-heavy",
    ] {
        if !tags.contains(required) {
            bail!("public corpus is missing required scenario tag {required}");
        }
    }
    Ok(())
}

fn validate_private_corpus(path: &Path, expected_hash: &str, require_external: bool) -> Result<()> {
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("private holdout is unavailable at {}", path.display()))?;
    if require_external {
        let repository = fs::canonicalize(env!("CARGO_MANIFEST_DIR"))?;
        if canonical.starts_with(repository) {
            bail!("private holdout must remain outside the repository checkout");
        }
    }
    let bytes = fs::read(&canonical)?;
    let actual_hash = blake3::hash(&bytes).to_hex().to_string();
    if actual_hash != expected_hash {
        bail!("private holdout hash mismatch; refusing a leaked or changed corpus");
    }
    let value: Value = serde_json::from_slice(&bytes)?;
    let count = value
        .get("cases")
        .and_then(Value::as_array)
        .map(Vec::len)
        .context("private holdout has no cases array")?;
    if count < 100 {
        bail!("private holdout must contain at least 100 cases; found {count}");
    }
    println!("validated private holdout: {count} cases, hash {actual_hash}");
    Ok(())
}

fn templates() -> Vec<Template> {
    vec![
        Template { id: "sql-interpolation", category: "Security", severity: "Critical", tags: &["security", "authorization-security", "suggestion-quality", "dependency-change"], file: "src/users.rs", line: Some(18), diff: "diff --git a/src/users.rs b/src/users.rs\n--- a/src/users.rs\n+++ b/src/users.rs\n@@ -17,2 +17,2 @@ pub fn find(name: &str) {\n-    db.query(\"select * from users where name = ?\", [name]);\n+    db.query(&format!(\"select * from users where name = '{name}'\"), []);\n }\n", context: &[], rationale: "User input becomes executable SQL.", minimal_fix: "Restore a bound query parameter.", accepted_fix_patterns: &["query(", "[name]"] },
        Template { id: "path-traversal", category: "Security", severity: "High", tags: &["security", "cross-directory", "suggestion-quality"], file: "src/files.rs", line: Some(12), diff: "diff --git a/src/files.rs b/src/files.rs\n--- a/src/files.rs\n+++ b/src/files.rs\n@@ -11,2 +11,2 @@ pub fn download(root: &Path, name: &str) {\n-    let path = safe_join(root, name)?;\n+    let path = root.join(name);\n     send(path)\n }\n", context: &[], rationale: "An attacker-controlled path may escape the download root.", minimal_fix: "Canonicalize and require containment or restore safe_join.", accepted_fix_patterns: &["safe_join", "starts_with"] },
        Template { id: "authorization-removal", category: "Security", severity: "Critical", tags: &["security", "authorization-security", "whole-file-deletion"], file: "src/admin.rs", line: Some(33), diff: "diff --git a/src/admin.rs b/src/admin.rs\n--- a/src/admin.rs\n+++ b/src/admin.rs\n@@ -31,4 +31,3 @@ pub fn delete_user(actor: &User, id: Id) {\n-    require_admin(actor)?;\n     users.delete(id)?;\n     Ok(())\n }\n", context: &[], rationale: "The privileged operation no longer checks authorization.", minimal_fix: "Restore the admin authorization guard before deletion.", accepted_fix_patterns: &["require_admin"] },
        Template { id: "n-plus-one", category: "Performance", severity: "High", tags: &["performance", "cross-layer"], file: "src/orders.rs", line: Some(21), diff: "diff --git a/src/orders.rs b/src/orders.rs\n--- a/src/orders.rs\n+++ b/src/orders.rs\n@@ -20,2 +20,5 @@ pub fn list() {\n-    repo.orders_with_customer()\n+    let orders = repo.orders();\n+    for order in &orders {\n+        order.customer = repo.customer(order.customer_id);\n+    }\n+    orders\n }\n", context: &[], rationale: "The new loop performs one customer query per order.", minimal_fix: "Load customers in the original joined/batched query.", accepted_fix_patterns: &["orders_with_customer", "batch"] },
        Template { id: "unbounded-retry", category: "Performance", severity: "High", tags: &["performance", "partial-migration"], file: "src/worker.rs", line: Some(28), diff: "diff --git a/src/worker.rs b/src/worker.rs\n--- a/src/worker.rs\n+++ b/src/worker.rs\n@@ -27,3 +27,3 @@ pub async fn run() {\n-    for _ in 0..MAX_ATTEMPTS {\n+    loop {\n         if poll().await? { break; }\n     }\n }\n", context: &[], rationale: "Persistent failure now keeps a worker alive forever.", minimal_fix: "Restore a bounded retry policy and terminal error.", accepted_fix_patterns: &["MAX_ATTEMPTS", "timeout"] },
        Template { id: "blocking-async", category: "Performance", severity: "Medium", tags: &["performance", "cross-layer"], file: "src/api.rs", line: Some(44), diff: "diff --git a/src/api.rs b/src/api.rs\n--- a/src/api.rs\n+++ b/src/api.rs\n@@ -43,2 +43,2 @@ pub async fn export() {\n-    tokio::fs::read(path).await?\n+    std::fs::read(path)?\n }\n", context: &[], rationale: "Blocking filesystem I/O stalls an async runtime worker.", minimal_fix: "Use async I/O or spawn_blocking.", accepted_fix_patterns: &["tokio::fs", "spawn_blocking"] },
        Template { id: "duplicate-parse", category: "Code Smell", severity: "Medium", tags: &["code-smell", "suggestion-quality"], file: "src/parser.rs", line: Some(12), diff: "diff --git a/src/parser.rs b/src/parser.rs\n--- a/src/parser.rs\n+++ b/src/parser.rs\n@@ -10,2 +10,4 @@ fn parse(input: &str) {\n-    let value = parse_once(input)?;\n+    validate(parse_once(input)?)?;\n+    let value = parse_once(input)?;\n+    audit(parse_once(input)?)?;\n     Ok(value)\n }\n", context: &[], rationale: "Parsing three times duplicates expensive work and can produce inconsistent results.", minimal_fix: "Parse once, then validate and audit the stored value.", accepted_fix_patterns: &["let value = parse_once", "validate(&value"] },
        Template { id: "partial-api-migration", category: "Code Smell", severity: "High", tags: &["code-smell", "partial-migration", "cross-file"], file: "src/service.rs", line: Some(14), diff: "diff --git a/src/model.rs b/src/model.rs\n--- a/src/model.rs\n+++ b/src/model.rs\n@@ -4,1 +4,1 @@\n-pub type UserId = u64;\n+pub struct UserId(pub String);\ndiff --git a/src/service.rs b/src/service.rs\n--- a/src/service.rs\n+++ b/src/service.rs\n@@ -13,2 +13,2 @@ pub fn load(raw: &str) {\n-    repo.find(UserId(raw.parse()?))\n+    repo.find(raw.parse::<u64>()?)\n }\n", context: &[], rationale: "The consumer still parses the migrated string ID as a number.", minimal_fix: "Construct the new string-backed UserId consistently.", accepted_fix_patterns: &["UserId(raw.to_owned", "UserId(raw.into"] },
        Template { id: "deep-policy-branch", category: "Code Smell", severity: "Medium", tags: &["code-smell", "large-file-structure"], file: "src/policy.rs", line: Some(51), diff: "diff --git a/src/policy.rs b/src/policy.rs\n--- a/src/policy.rs\n+++ b/src/policy.rs\n@@ -50,2 +50,8 @@ pub fn allowed(user: &User, item: &Item) {\n+    if user.active {\n+        if item.visible {\n+            if user.team == item.team {\n+                if !item.archived { return true; }\n+            }\n+        }\n+    }\n     false\n }\n", context: &[], rationale: "The new policy nests four conditions and obscures the access rule.", minimal_fix: "Use guard clauses or a named predicate.", accepted_fix_patterns: &["&&", "return false"] },
        Template { id: "serde-wire-name", category: "Convention", severity: "High", tags: &["convention", "dependency-change"], file: "src/config.rs", line: Some(8), diff: "diff --git a/src/config.rs b/src/config.rs\n--- a/src/config.rs\n+++ b/src/config.rs\n@@ -6,2 +6,3 @@ #[derive(Serialize)]\n-#[serde(rename_all = \"camelCase\")]\n pub struct ApiConfig {\n+    pub retry_count: u8,\n }\n", context: &[("AGENTS.md", "All serialized fields use camelCase to preserve the TypeScript wire format.")], rationale: "The new snake_case wire key violates the documented cross-language contract.", minimal_fix: "Restore the camelCase serde rename policy.", accepted_fix_patterns: &["rename_all = \"camelCase\""] },
        Template { id: "controller-database", category: "Convention", severity: "Medium", tags: &["convention", "cross-layer"], file: "src/controllers/users.rs", line: Some(19), diff: "diff --git a/src/controllers/users.rs b/src/controllers/users.rs\n--- a/src/controllers/users.rs\n+++ b/src/controllers/users.rs\n@@ -18,2 +18,2 @@ pub async fn show(id: Id) {\n-    users.show(id).await\n+    sqlx::query(\"select * from users where id = ?\").fetch_one(&POOL).await\n }\n", context: &[("AGENTS.md", "Controllers delegate all persistence to services; database access belongs in repositories.")], rationale: "The controller bypasses the repository boundary demonstrated by project conventions.", minimal_fix: "Delegate to the users service/repository.", accepted_fix_patterns: &["users.show", "repository"] },
        Template { id: "wrong-error-style", category: "Convention", severity: "Medium", tags: &["convention", "unconventional-layout"], file: "src/git/status.rs", line: Some(22), diff: "diff --git a/src/git/status.rs b/src/git/status.rs\n--- a/src/git/status.rs\n+++ b/src/git/status.rs\n@@ -21,2 +21,2 @@ pub async fn status() -> Result<Status> {\n-    command.output().await.map_err(GitError::from)?\n+    command.output().await.unwrap()\n }\n", context: &[("AGENTS.md", "Error handling: propagate with ? and map git stderr to GitErrorCode variants.")], rationale: "The new panic violates the repository's explicit recoverable git-error convention.", minimal_fix: "Propagate and map the subprocess error.", accepted_fix_patterns: &["map_err", "?"] },
        Template { id: "missing-boundary-test", category: "Test Quality", severity: "Medium", tags: &["test-quality", "weak-missing-tests"], file: "src/page.rs", line: Some(22), diff: "diff --git a/src/page.rs b/src/page.rs\n--- a/src/page.rs\n+++ b/src/page.rs\n@@ -20,2 +20,3 @@ pub fn size(requested: usize) {\n+    const MAX: usize = 100;\n+    requested.min(MAX)\n }\ndiff --git a/tests/page_test.rs b/tests/page_test.rs\n--- a/tests/page_test.rs\n+++ b/tests/page_test.rs\n@@ -4,1 +4,2 @@ fn small_size() {\n+    assert_eq!(size(20), 20);\n }\n", context: &[], rationale: "The new cap boundary and above-cap behavior are untested.", minimal_fix: "Assert size(100) and a value above 100.", accepted_fix_patterns: &[] },
        Template { id: "weak-error-assertion", category: "Test Quality", severity: "Medium", tags: &["test-quality", "multiline-assertion"], file: "src/auth.rs", line: Some(41), diff: "diff --git a/src/auth.rs b/src/auth.rs\n--- a/src/auth.rs\n+++ b/src/auth.rs\n@@ -40,2 +40,3 @@ pub fn auth(token: &str) {\n+    if token.is_empty() { return Err(Error::MissingToken); }\n     decode(token)\n }\ndiff --git a/spec/auth_spec.rs b/spec/auth_spec.rs\n--- a/spec/auth_spec.rs\n+++ b/spec/auth_spec.rs\n@@ -8,1 +8,4 @@ fn empty() {\n+    assert!(\n+        auth(\"\").is_err()\n+    );\n }\n", context: &[], rationale: "The test would pass for any error and does not protect the new MissingToken behavior.", minimal_fix: "Assert the exact error variant.", accepted_fix_patterns: &[] },
        Template { id: "deleted-regression-test", category: "Test Quality", severity: "High", tags: &["test-quality", "whole-file-deletion", "unconventional-layout"], file: "src/cache.rs", line: Some(15), diff: "diff --git a/src/cache.rs b/src/cache.rs\n--- a/src/cache.rs\n+++ b/src/cache.rs\n@@ -14,2 +14,3 @@ pub fn key(user: &str) {\n+    format!(\"user:{user}\")\n }\ndiff --git a/integration/cache_regression.rb b/integration/cache_regression.rb\ndeleted file mode 100644\n--- a/integration/cache_regression.rb\n+++ /dev/null\n@@ -1,3 +0,0 @@\n-test \"keys are isolated\" do\n-  refute_equal key(\"a\"), key(\"b\")\n-end\n", context: &[], rationale: "The only regression protection for user-isolated keys was deleted.", minimal_fix: "Retain an equivalent isolation test in the project's test layout.", accepted_fix_patterns: &[] },
        Template { id: "clean-documented-constant", category: "", severity: "", tags: &["false-positive-trap", "clean"], file: "src/constants.rs", line: None, diff: "diff --git a/src/constants.rs b/src/constants.rs\n--- a/src/constants.rs\n+++ b/src/constants.rs\n@@ -2,1 +2,3 @@\n use std::time::Duration;\n+// Shared client/server timeout.\n+pub const REQUEST_TIMEOUT_SECS: u64 = 30;\n", context: &[], rationale: "", minimal_fix: "", accepted_fix_patterns: &[] },
        Template { id: "clean-parameterized-test", category: "", severity: "", tags: &["false-positive-trap", "clean", "test-heavy"], file: "tests/slug_test.py", line: None, diff: "diff --git a/tests/slug_test.py b/tests/slug_test.py\n--- a/tests/slug_test.py\n+++ b/tests/slug_test.py\n@@ -4,1 +4,5 @@\n+@pytest.mark.parametrize(\"value, expected\", [(\"A B\", \"a-b\"), (\"x\", \"x\")])\n+def test_slug(value, expected):\n+    assert slug(value) == expected\n", context: &[], rationale: "", minimal_fix: "", accepted_fix_patterns: &[] },
        Template { id: "clean-safe-html", category: "", severity: "", tags: &["false-positive-trap", "clean", "security"], file: "src/view.ts", line: None, diff: "diff --git a/src/view.ts b/src/view.ts\n--- a/src/view.ts\n+++ b/src/view.ts\n@@ -5,1 +5,2 @@\n+const label = escapeHtml(user.label);\n+node.textContent = label;\n", context: &[], rationale: "", minimal_fix: "", accepted_fix_patterns: &[] },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_public_corpus_meets_size_shape_and_category_floor() {
        let corpus = build_corpus().unwrap();
        validate(&corpus).unwrap();
        assert_eq!(corpus.cases.len(), 118);
        assert!(corpus
            .cases
            .iter()
            .any(|case| case.source.kind == "historical-reviewed-defect"));
        for shape in [
            "small",
            "medium",
            "large",
            "test-heavy",
            "split-mode",
            "finding-heavy",
            "revision-heavy",
        ] {
            assert!(corpus.cases.iter().any(|case| case.shape == shape));
        }
    }

    #[test]
    fn convention_cases_include_objective_repository_evidence() {
        let corpus = build_corpus().unwrap();
        for case in corpus.cases.iter().filter(|case| {
            case.source.kind == "controlled-mutation"
                && case
                    .expected
                    .iter()
                    .any(|finding| finding.category == "Convention")
        }) {
            assert!(!case.review_input.context_files.is_empty(), "{}", case.id);
        }
    }

    #[test]
    fn anchors_include_added_lines_from_multiple_files() {
        let anchors = parse_valid_anchors(
            "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\ndiff --git a/b b/b\n--- a/b\n+++ b/b\n@@ -2,0 +3 @@\n+next\n",
        );
        assert_eq!(anchors["a"], BTreeSet::from([1]));
        assert_eq!(anchors["b"], BTreeSet::from([3]));
    }

    #[test]
    fn private_holdout_requires_hash_and_case_floor() {
        let directory =
            env::temp_dir().join(format!("wisetree-holdout-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir(&directory).unwrap();
        let path = directory.join("holdout.json");
        let cases = (0..100)
            .map(|index| serde_json::json!({"id": index}))
            .collect::<Vec<_>>();
        let bytes = serde_json::to_vec(&serde_json::json!({"cases": cases})).unwrap();
        fs::write(&path, &bytes).unwrap();
        let hash = blake3::hash(&bytes).to_hex().to_string();
        assert!(validate_private_corpus(&path, &hash, false).is_ok());
        assert!(validate_private_corpus(&path, "wrong", false).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reversed_historical_diffs_use_conventional_a_and_b_prefixes() {
        let normalized = normalize_reverse_diff_prefixes(
            "diff --git b/src/lib.rs a/src/lib.rs\n--- b/src/lib.rs\n+++ a/src/lib.rs\n",
        );
        assert_eq!(
            normalized,
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n"
        );
    }
}
