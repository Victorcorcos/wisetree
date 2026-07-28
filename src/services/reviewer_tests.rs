//! Deterministic changed-behavior to relevant-test coverage ledger.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use walkdir::{DirEntry, WalkDir};

const TEST_DISCOVERY_FILES_MAX: usize = 5_000;
const TEST_SOURCE_MAX_BYTES: u64 = 256 * 1024;
const ASSERTION_DIGEST_MAX_BYTES: usize = 4 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct ReviewCoverageInput {
    pub path: String,
    pub evidence: String,
    pub is_test: bool,
    pub deleted: bool,
    pub symbols: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ReviewCoverageLedger {
    pub entries: Vec<ReviewCoverageEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewCoverageEntry {
    pub id: String,
    pub application_path: String,
    pub behavior: String,
    pub changed_lines: String,
    pub tests: Vec<ReviewTestEvidence>,
    pub status: ReviewCoverageStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReviewTestEvidence {
    pub id: String,
    pub path: String,
    pub changed: bool,
    pub deleted: bool,
    pub assertion_digest: String,
    pub relationship: String,
    pub targeted_read_required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewCoverageStatus {
    AssertionsFound,
    Ambiguous,
    NoRelevantTest,
    LostProtection,
}

pub(crate) async fn build_coverage_ledger(
    root: PathBuf,
    inputs: Vec<ReviewCoverageInput>,
) -> ReviewCoverageLedger {
    tokio::task::spawn_blocking(move || build_coverage_ledger_blocking(&root, &inputs))
        .await
        .unwrap_or_default()
}

fn build_coverage_ledger_blocking(
    root: &Path,
    inputs: &[ReviewCoverageInput],
) -> ReviewCoverageLedger {
    let changed_tests = inputs
        .iter()
        .filter(|input| input.is_test)
        .map(|input| TestDocument {
            path: input.path.clone(),
            content: input.evidence.clone(),
            changed: true,
            deleted: input.deleted,
        })
        .collect::<Vec<_>>();
    let changed_paths = inputs
        .iter()
        .map(|input| input.path.as_str())
        .collect::<BTreeSet<_>>();
    let mut tests = changed_tests;
    for document in discover_unchanged_tests(root) {
        if !changed_paths.contains(document.path.as_str()) {
            tests.push(document);
        }
    }
    tests.sort_by(|left, right| left.path.cmp(&right.path));

    let mut full_body_owner = HashMap::<String, String>::new();
    let mut entries = Vec::new();
    for application in inputs.iter().filter(|input| !input.is_test) {
        let behaviors = if application.symbols.is_empty() {
            vec![format!("changed behavior in {}", application.path)]
        } else {
            application.symbols.clone()
        };
        for behavior in behaviors {
            let id = stable_id("behavior", &application.path, &behavior);
            let app_stem = Path::new(&application.path)
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            let behavior_lower = behavior.to_ascii_lowercase();
            let mut related = Vec::new();
            for test in &tests {
                let path_lower = test.path.to_ascii_lowercase();
                let content_lower = test.content.to_ascii_lowercase();
                let content_reference = meaningful(&behavior_lower)
                    && content_lower.contains(&behavior_lower)
                    || meaningful(&app_stem) && content_lower.contains(&app_stem);
                let name_reference = meaningful(&behavior_lower)
                    && path_lower.contains(&behavior_lower)
                    || meaningful(&app_stem) && path_lower.contains(&app_stem);
                if !content_reference && !name_reference {
                    continue;
                }
                let digest = assertion_digest(&test.content);
                let has_assertion = digest_has_assertion(&digest);
                let targeted_read_required = !content_reference || !has_assertion;
                let owner = full_body_owner
                    .entry(test.path.clone())
                    .or_insert_with(|| id.clone());
                let assertion_digest = if owner == &id {
                    digest
                } else {
                    format!(
                        "Assertion body owned by `{owner}`; targeted-read `{}` if this relationship is ambiguous.",
                        test.path
                    )
                };
                related.push(ReviewTestEvidence {
                    id: stable_id("test", &test.path, &behavior),
                    path: test.path.clone(),
                    changed: test.changed,
                    deleted: test.deleted,
                    assertion_digest,
                    relationship: if content_reference {
                        "symbol/import/reference".to_string()
                    } else {
                        "name/path only".to_string()
                    },
                    targeted_read_required,
                });
            }
            let status = if related.iter().any(|test| test.deleted) {
                ReviewCoverageStatus::LostProtection
            } else if related.is_empty() {
                ReviewCoverageStatus::NoRelevantTest
            } else if related.iter().any(|test| test.targeted_read_required) {
                ReviewCoverageStatus::Ambiguous
            } else {
                ReviewCoverageStatus::AssertionsFound
            };
            entries.push(ReviewCoverageEntry {
                id,
                application_path: application.path.clone(),
                behavior,
                changed_lines: changed_line_summary(&application.evidence),
                tests: related,
                status,
            });
        }
    }
    ReviewCoverageLedger { entries }
}

impl ReviewCoverageLedger {
    /// Render the behaviors owned by `application_paths`, stopping before
    /// `max_bytes` is exceeded. Every other prompt slot is capped; without a
    /// bound here one coverage prompt grew past a megabyte on a large PR and
    /// the call was rejected outright, so the whole group went unreviewed.
    /// Truncation is whole-behavior and the omitted ones are named, so the
    /// model still knows they exist and can target-read them.
    pub(crate) fn render_for_paths(
        &self,
        application_paths: &BTreeSet<&str>,
        tester_findings: &[(String, String)],
        max_bytes: usize,
    ) -> String {
        let findings = tester_findings.iter().fold(
            BTreeMap::<&str, Vec<&str>>::new(),
            |mut grouped, (path, title)| {
                grouped.entry(path).or_default().push(title);
                grouped
            },
        );
        let mut rendered = String::new();
        let mut omitted = BTreeSet::new();
        for entry in self
            .entries
            .iter()
            .filter(|entry| application_paths.contains(entry.application_path.as_str()))
        {
            if rendered.len() >= max_bytes {
                omitted.insert(entry.application_path.as_str());
                continue;
            }
            rendered.push_str(&format!(
                "### BEHAVIOR {}\n- application: `{}`\n- symbol/scenario: {}\n- changed anchors: {}\n- status: {}\n",
                entry.id,
                entry.application_path,
                entry.behavior,
                entry.changed_lines,
                entry.status.label()
            ));
            if entry.tests.is_empty() {
                rendered.push_str("- related tests: none found\n");
            }
            for test in &entry.tests {
                rendered.push_str(&format!(
                    "- test {}: `{}` [{}; {}; {}]\n{}\n",
                    test.id,
                    test.path,
                    if test.changed { "changed" } else { "unchanged" },
                    if test.deleted { "deleted" } else { "present" },
                    test.relationship,
                    indent(&test.assertion_digest)
                ));
                if test.targeted_read_required {
                    rendered.push_str(&format!(
                        "  TARGETED-READ-REQUIRED: inspect `{}` before emitting or suppressing this behavior's coverage finding.\n",
                        test.path
                    ));
                }
                if let Some(titles) = findings.get(test.path.as_str()) {
                    for title in titles {
                        rendered.push_str(&format!(
                            "  TESTER-FINDING: this protection is suspect — {title}\n"
                        ));
                    }
                }
            }
            rendered.push('\n');
        }
        if !omitted.is_empty() {
            rendered.push_str(&format!(
                "### BEHAVIORS OMITTED FOR SIZE (files: {})\n{}\n\
                 (their behavior evidence did not fit this prompt — read these files \
                 directly before judging their coverage)\n",
                omitted.len(),
                omitted
                    .iter()
                    .map(|path| format!("- `{path}`"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        if rendered.is_empty() {
            "(no application behaviors in this coverage group)".to_string()
        } else {
            rendered.trim_end().to_string()
        }
    }
}

impl ReviewCoverageStatus {
    fn label(self) -> &'static str {
        match self {
            Self::AssertionsFound => "meaningful assertion evidence found",
            Self::Ambiguous => "ambiguous — targeted real-file read required",
            Self::NoRelevantTest => "no relevant test found",
            Self::LostProtection => "lost protection — related test deleted",
        }
    }
}

#[derive(Debug)]
struct TestDocument {
    path: String,
    content: String,
    changed: bool,
    deleted: bool,
}

fn discover_unchanged_tests(root: &Path) -> Vec<TestDocument> {
    let mut documents = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(review_walk_entry)
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .take(TEST_DISCOVERY_FILES_MAX)
    {
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let path = relative.to_string_lossy().replace('\\', "/");
        if !looks_like_test(&path) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.len() > TEST_SOURCE_MAX_BYTES {
            continue;
        }
        let Ok(content) = fs::read_to_string(entry.path()) else {
            continue;
        };
        documents.push(TestDocument {
            path,
            content,
            changed: false,
            deleted: false,
        });
    }
    documents
}

fn review_walk_entry(entry: &DirEntry) -> bool {
    !entry.file_type().is_dir()
        || !matches!(
            entry.file_name().to_string_lossy().as_ref(),
            ".git" | "target" | "node_modules" | "vendor" | "dist" | "build"
        )
}

fn looks_like_test(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or_default();
    lower.split('/').any(|part| {
        matches!(
            part,
            "test"
                | "tests"
                | "spec"
                | "specs"
                | "__tests__"
                | "e2e"
                | "integration"
                | "features"
                | "cypress"
                | "acceptance"
        )
    }) || name.ends_with(".feature")
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.contains(".cy.")
        || name.starts_with("test_")
        || name.contains("_test.")
        || name.contains("_spec.")
        || name == "conftest.py"
}

fn assertion_digest(content: &str) -> String {
    let lines = content.lines().collect::<Vec<_>>();
    let mut keep = BTreeSet::new();
    for (index, line) in lines.iter().enumerate() {
        let lower = line.trim_start().to_ascii_lowercase();
        if is_scenario(&lower) || is_assertion(&lower) || is_parameterized(&lower) {
            keep.insert(index.saturating_sub(1));
            keep.insert(index);
            if index + 1 < lines.len() {
                keep.insert(index + 1);
            }
        }
    }
    let mut rendered = String::new();
    for index in keep {
        let line = format!("{:>6}  {}\n", index + 1, lines[index]);
        if rendered.len() + line.len() > ASSERTION_DIGEST_MAX_BYTES {
            break;
        }
        rendered.push_str(&line);
    }
    if rendered.is_empty() {
        "(no concrete scenario/assertion recognized)".to_string()
    } else {
        rendered.trim_end().to_string()
    }
}

fn is_scenario(line: &str) -> bool {
    line.starts_with("fn test_")
        || line.starts_with("async fn test_")
        || line.starts_with("def test_")
        || line.starts_with("it(")
        || line.starts_with("it ")
        || line.starts_with("test(")
        || line.starts_with("describe(")
        || line.starts_with("scenario:")
        || line.starts_with("scenario outline:")
}

fn is_assertion(line: &str) -> bool {
    line.contains("assert")
        || line.contains("expect(")
        || line.contains("is_expected")
        || line.contains("should")
        || line.contains("refute")
        || line.contains("pytest.raises")
}

fn is_parameterized(line: &str) -> bool {
    line.contains("parametrize")
        || line.contains("test_case")
        || line.contains("examples:")
        || line.contains("shared_examples")
}

fn digest_has_assertion(digest: &str) -> bool {
    digest
        .lines()
        .any(|line| is_assertion(&line.to_ascii_lowercase()))
}

fn changed_line_summary(evidence: &str) -> String {
    let lines = evidence
        .lines()
        .filter(|line| line.as_bytes().get(7) == Some(&b'+'))
        .filter_map(|line| line.trim_start().split_once(' ')?.0.parse::<u64>().ok())
        .collect::<Vec<_>>();
    match (lines.first(), lines.last()) {
        (Some(first), Some(last)) if first != last => format!("{first}-{last}"),
        (Some(line), _) => line.to_string(),
        _ => "file-level/deletion".to_string(),
    }
}

fn stable_id(kind: &str, owner: &str, value: &str) -> String {
    let digest = blake3::hash(format!("{kind}\0{owner}\0{value}").as_bytes());
    format!("{kind}:{}", &digest.to_hex()[..12])
}

fn meaningful(value: &str) -> bool {
    value.len() >= 3 && !matches!(value, "lib" | "main" | "mod" | "index")
}

fn indent(value: &str) -> String {
    value
        .lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(path: &str, evidence: &str, is_test: bool, deleted: bool) -> ReviewCoverageInput {
        ReviewCoverageInput {
            path: path.to_string(),
            evidence: evidence.to_string(),
            is_test,
            deleted,
            symbols: if is_test {
                Vec::new()
            } else {
                vec!["authenticate".to_string()]
            },
        }
    }

    #[test]
    fn discovers_unchanged_tests_in_separate_roots_and_keeps_assertions() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("integration")).unwrap();
        fs::write(
            root.path().join("integration/auth_contract.py"),
            "@pytest.mark.parametrize('token', ['', 'bad'])\ndef test_authenticate(token):\n    assert authenticate(token).is_err()\n",
        )
        .unwrap();
        let ledger = build_coverage_ledger_blocking(
            root.path(),
            &[input(
                "src/auth.rs",
                "    10 +authenticate(token)",
                false,
                false,
            )],
        );
        assert_eq!(
            ledger.entries[0].status,
            ReviewCoverageStatus::AssertionsFound
        );
        assert!(!ledger.entries[0].tests[0].changed);
        assert!(ledger.entries[0].tests[0]
            .assertion_digest
            .contains("parametrize"));
        assert!(ledger.entries[0].tests[0]
            .assertion_digest
            .contains("assert"));
    }

    #[test]
    fn name_only_test_is_ambiguous_and_requires_targeted_read() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("tests")).unwrap();
        fs::write(
            root.path().join("tests/authenticate_test.rs"),
            "fn unrelated() {}\n",
        )
        .unwrap();
        let ledger = build_coverage_ledger_blocking(
            root.path(),
            &[input(
                "src/auth.rs",
                "    10 +authenticate(token)",
                false,
                false,
            )],
        );
        assert_eq!(ledger.entries[0].status, ReviewCoverageStatus::Ambiguous);
        assert!(ledger.entries[0].tests[0].targeted_read_required);
    }

    #[test]
    fn deleted_changed_test_records_lost_protection() {
        let root = tempfile::tempdir().unwrap();
        let ledger = build_coverage_ledger_blocking(
            root.path(),
            &[
                input("src/auth.rs", "    10 +authenticate(token)", false, false),
                input(
                    "features/auth.feature",
                    "Scenario: rejects bad token\nassert authenticate(token)",
                    true,
                    true,
                ),
            ],
        );
        assert_eq!(
            ledger.entries[0].status,
            ReviewCoverageStatus::LostProtection
        );
    }

    #[test]
    fn one_test_body_has_one_owner_across_behaviors() {
        let root = tempfile::tempdir().unwrap();
        let mut application = input(
            "src/auth.rs",
            "    10 +authenticate(token)\n    20 +authorize(user)",
            false,
            false,
        );
        application.symbols = vec!["authenticate".to_string(), "authorize".to_string()];
        let ledger = build_coverage_ledger_blocking(
            root.path(),
            &[
                application,
                input(
                    "tests/auth_test.rs",
                    "fn auth() { assert!(authenticate(token)); assert!(authorize(user)); }",
                    true,
                    false,
                ),
            ],
        );
        let bodies = ledger
            .entries
            .iter()
            .flat_map(|entry| &entry.tests)
            .filter(|test| test.assertion_digest.contains("fn auth"))
            .count();
        assert_eq!(bodies, 1);
    }

    #[test]
    fn no_test_repository_is_explicit() {
        let root = tempfile::tempdir().unwrap();
        let ledger = build_coverage_ledger_blocking(
            root.path(),
            &[input(
                "src/auth.rs",
                "    10 +authenticate(token)",
                false,
                false,
            )],
        );
        assert_eq!(
            ledger.entries[0].status,
            ReviewCoverageStatus::NoRelevantTest
        );
        assert!(ledger.entries[0].tests.is_empty());
    }

    /// The ledger is one behavior block per changed symbol and was the only
    /// uncapped slot in the coverage prompt — on a large PR it grew past a
    /// megabyte and the model refused the call. It now stops at its budget
    /// and names the files it dropped so they can still be read.
    #[test]
    fn ledger_rendering_stops_at_its_budget_and_names_what_it_dropped() {
        let root = tempfile::tempdir().unwrap();
        let inputs = (0..60)
            .map(|index| {
                input(
                    &format!("src/service{index}.rs"),
                    "    10 +authenticate(token)",
                    false,
                    false,
                )
            })
            .collect::<Vec<_>>();
        let ledger = build_coverage_ledger_blocking(root.path(), &inputs);
        let paths = inputs
            .iter()
            .map(|input| input.path.as_str())
            .collect::<BTreeSet<_>>();

        let full = ledger.render_for_paths(&paths, &[], usize::MAX);
        assert!(!full.contains("BEHAVIORS OMITTED FOR SIZE"));

        let capped = ledger.render_for_paths(&paths, &[], full.len() / 2);
        assert!(capped.len() < full.len());
        assert!(capped.contains("BEHAVIORS OMITTED FOR SIZE"));
        assert!(capped.contains("`src/service59.rs`"));
        // Whole behaviors only — a rendered block is never cut in half.
        assert_eq!(
            capped.matches("- application: `").count(),
            capped.matches("### BEHAVIOR ").count()
        );
    }
}
