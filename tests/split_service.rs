use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use once_cell::sync::Lazy;
use tempfile::TempDir;
use tokio::sync::{oneshot, Mutex};
use wisetree::config::schema::DashboardConfig;
use wisetree::services::{
    build_corrective_plan_prompt, build_split_open_prompt, build_split_plan_prompt,
    compose_split_body, describe_snapshot_changes, final_split_title, inventory_diff,
    parse_materialization, parse_numstat_totals, parse_publication, parse_split_draft,
    parse_split_drafting, parse_split_plan, parse_split_plan_transcript, parse_split_run,
    patch_for_units, provisional_split_title, render_publication, render_split_drafting,
    render_split_plan, split_draft_cache_path, split_draft_job_id, split_layer_sizes,
    validate_split_body, validate_split_manifest, validate_split_publication,
    validate_split_resume, ChangeUnit, ChangeUnitKind, DashboardService, SplitDraft,
    SplitDraftRecord, SplitDraftingRecord, SplitIdentity, SplitPlan, SplitPreflight,
    SplitPublication, SplitPublishedPullRequest, SplitRepositorySnapshot, SplitResponsibility,
};

mod support;

use support::{git, init_repo_with_main};

static HOME_LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

fn make_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }
}

fn git_stdout(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_string()
}

struct RepoFixture {
    _parent: TempDir,
    repo: std::path::PathBuf,
    source: std::path::PathBuf,
    base: String,
    head: String,
}

fn split_repo(initial: &[(&str, &[u8])], changed: &[(&str, Option<&[u8]>)]) -> RepoFixture {
    let parent = tempfile::tempdir().unwrap();
    let repo = parent.path().join("repo");
    let source = parent.path().join("repo-feature");
    fs::create_dir_all(&repo).unwrap();
    init_repo_with_main(&repo);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    for (path, contents) in initial {
        let absolute = repo.join(path);
        fs::create_dir_all(absolute.parent().unwrap()).unwrap();
        fs::write(absolute, contents).unwrap();
    }
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let base = git_stdout(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["update-ref", "refs/remotes/origin/main", &base]);
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "feature",
            source.to_str().unwrap(),
            "main",
        ],
    );
    for (path, contents) in changed {
        let absolute = source.join(path);
        if let Some(contents) = contents {
            fs::create_dir_all(absolute.parent().unwrap()).unwrap();
            fs::write(absolute, contents).unwrap();
        } else {
            fs::remove_file(absolute).unwrap();
        }
    }
    git(&source, &["add", "-A"]);
    git(&source, &["commit", "-q", "-m", "source"]);
    let head = git_stdout(&source, &["rev-parse", "HEAD"]);
    RepoFixture {
        _parent: parent,
        repo,
        source,
        base,
        head,
    }
}

fn fixture() -> SplitPreflight {
    let unit = |id: &str, path: &str, additions: u64, deletions: u64| ChangeUnit {
        id: id.to_string(),
        path: path.to_string(),
        old_path: None,
        kind: ChangeUnitKind::TextHunk,
        old_start: Some(1),
        old_lines: Some(deletions),
        new_start: Some(1),
        new_lines: Some(additions),
        additions,
        deletions,
        line_counts_available: true,
    };
    SplitPreflight {
        worktree_path: "/tmp/worktree".to_string(),
        identity: SplitIdentity {
            repository: "owner/repo".to_string(),
            remote: "origin".to_string(),
            base_ref: "origin/main".to_string(),
            base_sha: "base".to_string(),
            source_branch: "feature".to_string(),
            source_head: "head".to_string(),
            max: 10,
            additions: 8,
            deletions: 2,
        },
        units: vec![
            unit("CU0001", "src/a.rs", 3, 1),
            unit("CU0002", "tests/a_test.rs", 1, 0),
            unit("CU0003", "src/b.rs", 2, 1),
            unit("CU0004", "tests/b_test.rs", 2, 0),
        ],
    }
}

#[test]
fn provisional_titles_normalize_ticket_and_ticketless_branches() {
    assert_eq!(
        provisional_split_title(
            "duv4091_save_work_orders_in_equinor_during_authorization",
            4,
            7
        ),
        "DUV-4091 Save Work Orders In Equinor During Authorization (4/7)"
    );
    assert_eq!(
        provisional_split_title("feature-improve_cache-health", 2, 3),
        "Feature Improve Cache Health (2/3)"
    );
    assert_eq!(provisional_split_title("duv-4091", 1, 2), "DUV-4091 (1/2)");
}

#[test]
fn durable_run_and_drafting_records_drive_exact_resume() {
    let preflight = fixture();
    let plan = parse_split_plan(valid_response(), &preflight).unwrap();
    let mut document = render_split_plan(&preflight, &plan, "awaiting approval");
    let run = parse_split_run(&document).unwrap().unwrap();
    assert_eq!(run.identity, preflight.identity);
    assert_eq!(run.units, preflight.units);
    assert_eq!(run.plan, plan);
    assert_eq!(run.status, "awaiting approval");
    assert!(validate_split_resume(&document, &preflight).is_ok());

    let record = SplitDraftRecord {
        job_id: "head-layer-1-pr-41".into(),
        source_head: "head".into(),
        order: 1,
        pr_number: 41,
        pr_url: "https://github.com/owner/repo/pull/41".into(),
        correction_attempted: false,
        draft: None,
        final_title: None,
        final_body: None,
        applied: false,
        error: Some("retry this PR only".into()),
    };
    document.push_str(
        &render_split_drafting(&SplitDraftingRecord {
            records: vec![record.clone()],
            completed: false,
        })
        .unwrap(),
    );
    let drafting = parse_split_drafting(&document).unwrap().unwrap();
    assert_eq!(drafting.records, vec![record]);
    assert!(!drafting.completed);
    assert!(document.contains("retry resumes cached work"));

    let mut mismatch = preflight.clone();
    mismatch.identity.max += 1;
    let error = validate_split_resume(&document, &mismatch)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("does not match the live repository"),
        "{error}"
    );
    assert!(
        error.contains("did not touch recorded artifacts"),
        "{error}"
    );

    let corrupt = document.replace(
        "<!-- wisetree-split-run {",
        "<!-- wisetree-split-run not-json{",
    );
    assert!(parse_split_run(&corrupt)
        .unwrap_err()
        .to_string()
        .contains("record is corrupt"));
}

#[test]
fn publication_record_round_trips_complete_verified_identity() {
    let publication = SplitPublication {
        repository: "owner/repo".into(),
        trunk: "main".into(),
        source_branch: "duv4091_change".into(),
        stack_link_completed: true,
        status: "published, verified, and provisionally titled".into(),
        diagnostics: None,
        pull_requests: vec![SplitPublishedPullRequest {
            order: 1,
            branch: "duv4091_change.1_foundation".into(),
            expected_base: "main".into(),
            number: 41,
            url: "https://github.com/owner/repo/pull/41".into(),
            provisional_title: "DUV-4091 Change (1/2)".into(),
            provisional_title_applied: true,
        }],
    };
    let rendered = render_publication(&publication).unwrap();
    assert_eq!(parse_publication(&rendered).unwrap(), Some(publication));
    assert!(rendered.contains("Expected base"));
    assert!(rendered.contains("title applied") || rendered.contains("yes"));
}

fn published_stack() -> SplitPublication {
    SplitPublication {
        repository: "owner/repo".into(),
        trunk: "main".into(),
        source_branch: "duv4091_change".into(),
        stack_link_completed: true,
        status: "published, verified, and provisionally titled".into(),
        diagnostics: None,
        pull_requests: (1..=3)
            .map(|order| SplitPublishedPullRequest {
                order,
                branch: if order == 3 {
                    "duv4091_change".into()
                } else {
                    format!("duv4091_change.{order}_layer")
                },
                expected_base: match order {
                    1 => "main".into(),
                    2 => "duv4091_change.1_layer".into(),
                    _ => "duv4091_change.2_layer".into(),
                },
                number: 40 + order as u64,
                url: format!("https://github.com/owner/repo/pull/{}", 40 + order),
                provisional_title: format!("DUV-4091 Change ({order}/3)"),
                provisional_title_applied: true,
            })
            .collect(),
    }
}

#[test]
fn split_draft_contract_prompt_and_title_keep_ai_out_of_bookkeeping() {
    let responsibility = SplitResponsibility {
        order: 1,
        name: "Foundation".into(),
        branch_slug: "foundation".into(),
        rationale: "Introduces the shared contract.".into(),
        units: vec!["CU0001".into()],
        test_units: vec!["CU0001".into()],
        paths: vec!["tests/a.rs".into()],
    };
    let prompt = build_split_open_prompt(
        &responsibility,
        "DUV-4091",
        "commit",
        "diff",
        "# Description ✍️",
    );
    assert!(prompt.contains("Do not create branches, commits, pushes, files"));
    assert!(prompt.contains("Do not run git or gh"));
    assert!(!prompt.contains("https://github.com/owner/repo/pull/41"));

    let draft = parse_split_draft(
        r#"{"title_summary":"2. DUV-4091 Add the contract (9/9)","description_content":"Adds the contract and explains how to test it."}"#,
    )
    .unwrap();
    assert_eq!(
        final_split_title("duv4091_change", &draft.title_summary, 2, 3).unwrap(),
        "DUV-4091 Add the contract (2/3)"
    );
    assert!(parse_split_draft(
        r#"{"title_summary":"x","description_content":"See https://example.test"}"#
    )
    .is_err());
}

#[test]
fn split_body_is_deterministic_and_preserves_non_description_template_sections() {
    let publication = published_stack();
    let template = "# Description ✍️\n\nplaceholder\n\n# Overview 🔍\n\nkeep overview\n\n# Test Guidance 🦮\n\nkeep tests\n\n# Custom\n\nkeep custom";
    let body = compose_split_body(
        template,
        "This layer introduces the shared contract.",
        &publication.pull_requests,
        2,
    )
    .unwrap();
    validate_split_body(&body, &publication.pull_requests, 2).unwrap();
    assert!(body.contains("1. https://github.com/owner/repo/pull/41\n"));
    assert!(body.contains("2. https://github.com/owner/repo/pull/42 **(current PR)**"));
    assert!(body.contains("3. https://github.com/owner/repo/pull/43 **(future PR)**"));
    assert!(body.find("### Split Plan 📋").unwrap() < body.find("This layer").unwrap());
    assert!(body.contains("keep overview"));
    assert!(body.contains("keep tests"));
    assert!(body.contains("keep custom"));
    assert!(!body.contains("placeholder"));

    assert!(compose_split_body(
        "# Description\n\na\n# Description ✍️\n\nb",
        "prose",
        &publication.pull_requests,
        1,
    )
    .is_err());
    let without = compose_split_body(
        "# Overview\n\nmedia stays",
        "prose",
        &publication.pull_requests,
        1,
    )
    .unwrap();
    assert!(without.starts_with("# Description ✍️\n\n### Split Plan 📋"));
    assert!(without.contains("# Overview\n\nmedia stays"));
}

#[test]
fn publication_gate_rejects_missing_or_misordered_canonical_urls() {
    let mut preflight = fixture();
    preflight.identity.repository = "owner/repo".into();
    preflight.identity.source_branch = "duv4091_change".into();
    let plan = parse_split_plan(valid_response(), &preflight).unwrap();
    let mut publication = published_stack();
    publication.pull_requests.truncate(2);
    publication.pull_requests[1].branch = "duv4091_change".into();
    assert!(validate_split_publication(&preflight, &plan, &publication).is_ok());
    publication.pull_requests.swap(0, 1);
    assert!(validate_split_publication(&preflight, &plan, &publication).is_err());
    publication.pull_requests[0].url = "https://github.com/owner/repo/pull/999".into();
    assert!(validate_split_publication(&preflight, &plan, &publication).is_err());
}

#[test]
fn numstat_totals_cover_text_and_ignore_unavailable_binary_counts() {
    let input = "3\t2\tsrc/a.rs\0-\t-\tlogo.png\0\x31\t0\t\0old.rs\0new.rs\0";
    assert_eq!(parse_numstat_totals(input), (4, 2));
}

fn valid_response() -> &'static str {
    r#"{"responsibilities":[{"order":1,"name":"Foundation","branch_slug":"foundation","rationale":"Builds directly on the resolved base.","units":["CU0001","CU0002"],"test_units":["CU0002"],"paths":["src/a.rs","tests/a_test.rs"]},{"order":2,"name":"Consumer","branch_slug":"consumer","rationale":"Uses the foundation behavior.","units":["CU0003","CU0004"],"test_units":["CU0004"],"paths":["src/b.rs","tests/b_test.rs"]}]}"#
}

#[test]
fn inventory_uses_hunks_but_keeps_rename_mode_and_binary_atomic() {
    let diff = r#"diff --git a/src/a.rs b/src/a.rs
index 1111111..2222222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1 +1,2 @@
-old
+new
+more
@@ -10,2 +11 @@
-gone
-gone too
+kept
diff --git a/old.txt b/new.txt
similarity index 80%
rename from old.txt
rename to new.txt
--- a/old.txt
+++ b/new.txt
@@ -1 +1 @@
-before
+after
diff --git a/run.sh b/run.sh
old mode 100644
new mode 100755
diff --git a/logo.png b/logo.png
new file mode 100644
index 0000000..3333333
Binary files /dev/null and b/logo.png differ
"#;
    let units = inventory_diff(diff).expect("inventory");
    assert_eq!(units.len(), 5);
    assert_eq!(units[0].id, "CU0001");
    assert_eq!((units[0].additions, units[0].deletions), (2, 1));
    assert_eq!((units[1].additions, units[1].deletions), (1, 2));
    assert_eq!(units[2].kind, ChangeUnitKind::Rename);
    assert_eq!(units[2].old_path.as_deref(), Some("old.txt"));
    assert_eq!(units[3].kind, ChangeUnitKind::ModeChange);
    assert_eq!(units[4].kind, ChangeUnitKind::Binary);
    assert!(!units[4].line_counts_available);
}

#[test]
fn selected_patch_keeps_only_assigned_hunks_from_a_shared_file() {
    let diff = r#"diff --git a/tests/shared.txt b/tests/shared.txt
index 1111111..2222222 100644
--- a/tests/shared.txt
+++ b/tests/shared.txt
@@ -1 +1 @@
-one
+ONE
@@ -5 +5 @@
-five
+FIVE
"#;
    let patch = patch_for_units(diff, &BTreeSet::from(["CU0001".to_string()])).unwrap();
    assert!(patch.contains("-one\n+ONE"));
    assert!(!patch.contains("-five"));
    assert_eq!(patch.matches("diff --git ").count(), 1);
}

#[test]
fn strict_plan_parser_accepts_complete_srp_assignment_and_renders_integrity() {
    let preflight = fixture();
    validate_split_manifest(&preflight.identity, &preflight.units).unwrap();
    let plan = parse_split_plan(valid_response(), &preflight).expect("valid plan");
    let rendered = render_split_plan(&preflight, &plan, "awaiting approval");
    assert!(rendered.contains("owner/repo"));
    assert!(rendered.contains("Status: **awaiting approval**"));
    assert!(rendered.contains("| CU0004 | TextHunk | `tests/b_test.rs`"));
    assert!(rendered.contains("| 2 | Consumer | `consumer`"));
}

/// A Rails/RSpec branch (`spec/**/*_spec.rb`) and a layer that legitimately
/// changes no tests at all must both plan successfully: the harness classifies
/// tests, so the planner cannot be wrong about them.
#[test]
fn harness_derives_test_units_for_every_ecosystem_and_testless_layers() {
    let mut preflight = fixture();
    preflight.units[1].path = "spec/models/user_spec.rb".to_string();
    preflight.units[3].path = "db/migrate/001_add_column.rb".to_string();
    let response = r#"{"responsibilities":[{"order":1,"name":"Model","branch_slug":"model","rationale":"Builds directly on the resolved base.","units":["CU0001","CU0002"],"paths":["src/a.rs","spec/models/user_spec.rb"]},{"order":2,"name":"Migration","branch_slug":"migration","rationale":"Persists what the model expects.","units":["CU0003","CU0004"],"paths":["src/b.rs","db/migrate/001_add_column.rb"]}]}"#;
    let plan = parse_split_plan(response, &preflight).expect("rails-shaped plan");
    assert_eq!(plan.responsibilities[0].test_units, vec!["CU0002"]);
    assert!(plan.responsibilities[1].test_units.is_empty());

    // A stale `test_units` from the AI is overwritten, never trusted.
    let lying = response.replace(
        r#""units":["CU0003","CU0004"]"#,
        r#""units":["CU0003","CU0004"],"test_units":["CU0003"]"#,
    );
    assert!(parse_split_plan(&lying, &preflight)
        .expect("declared test units are ignored")
        .responsibilities[1]
        .test_units
        .is_empty());

    let prompt = build_split_plan_prompt(&preflight, None, None);
    assert!(
        prompt.contains("spec/models/user_spec.rb | test"),
        "{prompt}"
    );
    assert!(
        prompt.contains("db/migrate/001_add_column.rb | implementation"),
        "{prompt}"
    );
}

#[test]
fn test_classification_covers_common_layouts_without_matching_lookalike_words() {
    let response_for = |path: &str| {
        valid_response()
            .replace("tests/a_test.rs", path)
            .to_string()
    };
    let mut preflight = fixture();
    for path in [
        "spec/models/user_spec.rb",
        "test/user_test.rb",
        "internal/handler_test.go",
        "api/test_client.py",
        "conftest.py",
        "src/App.test.tsx",
        "src/App.spec.jsx",
        "lib/parser_test.exs",
        "src/main/java/PaymentTest.java",
        "Sources/PaymentTests.swift",
        "features/login.feature",
        "__tests__/render.js",
        "e2e/checkout.ts",
        "cypress/e2e/login.js",
    ] {
        preflight.units[1].path = path.to_string();
        let plan = parse_split_plan(&response_for(path), &preflight).expect("valid plan");
        assert_eq!(
            plan.responsibilities[0].test_units,
            vec!["CU0002"],
            "expected `{path}` to be classified as a test"
        );
    }
    for path in [
        "src/latest.rb",
        "app/models/protest.rb",
        "src/testament.py",
        "config/routes.rb",
    ] {
        preflight.units[1].path = path.to_string();
        let plan = parse_split_plan(&response_for(path), &preflight).expect("valid plan");
        assert!(
            plan.responsibilities[0].test_units.is_empty(),
            "expected `{path}` to be classified as implementation"
        );
    }
}

/// The live planning terminal shows the harness's own chrome, so the contract
/// object arrives fenced or wrapped in a closing remark. The transcript reader
/// isolates it and then applies the identical strict contract.
#[test]
fn transcript_reader_isolates_the_contract_from_live_planner_chrome() {
    let preflight = fixture();
    let expected = parse_split_plan(valid_response(), &preflight).unwrap();

    for transcript in [
        valid_response().to_string(),
        format!("Here is the split:\n\n```json\n{}\n```\n", valid_response()),
        format!("```\n{}\n```", valid_response()),
        format!(
            "Thinking done.\n{}\nLet me know if you want changes.",
            valid_response()
        ),
    ] {
        assert_eq!(
            parse_split_plan_transcript(&transcript, &preflight).expect("contract found"),
            expected,
            "failed on: {transcript}"
        );
    }

    let error = parse_split_plan_transcript("I could not split this branch.", &preflight)
        .expect_err("no JSON object")
        .to_string();
    assert!(error.contains("no JSON object"), "{error}");
    // A transcript whose object is present but invalid still fails the same
    // contract, so the corrective retry keeps its exact meaning.
    assert!(parse_split_plan_transcript(
        &format!(
            "```json\n{}\n```",
            valid_response().replace("\"CU0003\"", "\"CU9999\"")
        ),
        &preflight
    )
    .is_err());
}

#[test]
fn parser_rejects_prose_unknown_duplicate_missing_and_ai_arithmetic() {
    let preflight = fixture();
    assert!(parse_split_plan(&format!("proposal: {}", valid_response()), &preflight).is_err());
    assert!(parse_split_plan(
        &valid_response().replace("\"CU0003\"", "\"CU9999\""),
        &preflight
    )
    .is_err());
    assert!(parse_split_plan(
        &valid_response().replace("\"CU0003\",\"CU0004\"", "\"CU0001\",\"CU0004\""),
        &preflight
    )
    .is_err());
    assert!(parse_split_plan(
        &valid_response().replace(",\"paths\":", ",\"additions\":4,\"paths\":"),
        &preflight
    )
    .is_err());
    assert!(parse_split_plan(
        &valid_response().replace(",\"CU0004\"],\"test_units\"", "],\"test_units\""),
        &preflight
    )
    .is_err());
}

/// MAX yields to the semantic boundary: an oversized responsibility — even an
/// indivisible change larger than MAX on its own — plans, sizes and renders
/// with its overflow and its reason instead of failing.
#[test]
fn max_is_a_soft_ceiling_that_reports_the_overflow_and_its_reason() {
    let mut preflight = fixture();
    preflight.identity.max = 3;
    preflight.units[0].kind = ChangeUnitKind::Binary;
    validate_split_manifest(&preflight.identity, &preflight.units)
        .expect("an indivisible change over MAX no longer blocks the split");

    let plan = parse_split_plan(valid_response(), &preflight).expect("oversized plan is accepted");
    let sizes = split_layer_sizes(&preflight, &plan);
    assert_eq!(sizes.len(), 2);
    assert_eq!((sizes[0].additions, sizes[0].deletions), (4, 1));
    assert_eq!(sizes[0].changed, 5);
    assert!(sizes[0].over_max());
    assert_eq!(sizes[0].overflow(), 2);
    assert!(sizes[1].over_max());

    preflight.identity.max = 10;
    let sizes = split_layer_sizes(&preflight, &plan);
    assert!(!sizes[0].over_max() && !sizes[1].over_max());
    assert_eq!(sizes[0].overflow(), 0);

    preflight.identity.max = 3;
    let rendered = render_split_plan(&preflight, &plan, "awaiting approval");
    assert!(rendered.contains("| 5 | over by 2 |"), "{rendered}");
    assert!(rendered.contains("[!WARNING]"), "{rendered}");
    assert!(
        rendered.contains("kept whole because splitting it would break the single responsibility"),
        "{rendered}"
    );
}

#[test]
fn prompt_contains_frozen_manifest_and_revision_only_when_both_inputs_exist() {
    let preflight = fixture();
    let first = build_split_plan_prompt(&preflight, None, None);
    assert!(first.contains("origin/main"));
    assert!(first.contains("CU0001 | TextHunk | src/a.rs | implementation | +3 -1"));
    assert!(first.contains("CU0002 | TextHunk | tests/a_test.rs | test | +1 -0"));
    assert!(first.contains("Do not write files"));
    let incomplete_revision = build_split_plan_prompt(&preflight, Some("stale"), None);
    assert!(!incomplete_revision.contains("stale"));

    let revision = build_split_plan_prompt(&preflight, Some(valid_response()), Some("move B"));
    assert!(revision.contains(valid_response()));
    assert!(revision.contains("move B"));
}

#[test]
fn corrective_prompt_only_reasserts_the_unchanged_contract_failure() {
    let original = build_split_plan_prompt(&fixture(), None, None);
    let corrected = build_corrective_plan_prompt(&original, "unknown unit CU9999");
    assert!(corrected.starts_with(&original));
    assert!(corrected.contains("unknown unit CU9999"));
    assert!(corrected.contains("same schema"));
    assert!(!corrected.contains("try a different approach"));
}

#[test]
fn repository_snapshot_diff_names_head_refs_status_and_relevant_files() {
    let snapshot = SplitRepositorySnapshot {
        status: String::new(),
        head: "head-1".into(),
        refs: "refs/heads/feature head-1".into(),
        files: vec![("src/a.rs".into(), Some(b"before".to_vec()))],
    };
    assert!(describe_snapshot_changes(&snapshot, &snapshot).is_empty());

    let mut changed = snapshot.clone();
    changed.status = " M src/a.rs".into();
    changed.head = "head-2".into();
    changed.refs = "refs/heads/feature head-2".into();
    changed.files[0].1 = Some(b"after".to_vec());
    let changes = describe_snapshot_changes(&snapshot, &changed).join("\n");
    assert!(changes.contains("HEAD"), "{changes}");
    assert!(changes.contains("refs"), "{changes}");
    assert!(changes.contains("status"), "{changes}");
    assert!(changes.contains("src/a.rs"), "{changes}");
}

#[tokio::test(flavor = "current_thread")]
async fn materializes_shared_file_hunks_without_touching_the_source() {
    let _guard = HOME_LOCK.lock().await;
    let home = tempfile::tempdir().unwrap();
    let previous_home = std::env::var_os("HOME");
    std::env::set_var("HOME", home.path());

    let original = b"one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\n";
    let changed = b"ONE\ntwo\nthree\nfour\nFIVE\nsix\nseven\neight\nNINE\n";
    let fixture = split_repo(
        &[("tests/shared.txt", original)],
        &[("tests/shared.txt", Some(changed))],
    );
    let diff = git_stdout(
        &fixture.source,
        &[
            "diff",
            "--binary",
            "--full-index",
            "--find-renames",
            "--unified=0",
            "--no-color",
            "--no-ext-diff",
            &format!("{}..{}", fixture.base, fixture.head),
        ],
    );
    let units = inventory_diff(&diff).unwrap();
    assert_eq!(units.len(), 3);
    let preflight = SplitPreflight {
        worktree_path: fixture.source.to_string_lossy().into_owned(),
        identity: SplitIdentity {
            repository: "example/repo".into(),
            remote: "origin".into(),
            base_ref: "origin/main".into(),
            base_sha: fixture.base.clone(),
            source_branch: "feature".into(),
            source_head: fixture.head.clone(),
            max: 10,
            additions: 3,
            deletions: 3,
        },
        units,
    };
    let response = r#"{"responsibilities":[{"order":1,"name":"First hunk","branch_slug":"first-hunk","rationale":"Independent first-line behavior.","units":["CU0001"],"test_units":["CU0001"],"paths":["tests/shared.txt"]},{"order":2,"name":"Second hunk","branch_slug":"second-hunk","rationale":"Independent middle-line behavior.","units":["CU0002"],"test_units":["CU0002"],"paths":["tests/shared.txt"]},{"order":3,"name":"Third hunk","branch_slug":"third-hunk","rationale":"Independent last-line behavior.","units":["CU0003"],"test_units":["CU0003"],"paths":["tests/shared.txt"]}]}"#;
    let plan = parse_split_plan(response, &preflight).unwrap();
    let source_before = fs::read(fixture.source.join("tests/shared.txt")).unwrap();
    let service = DashboardService::new(fixture.repo.clone(), DashboardConfig::default());
    service
        .save_split_plan(&preflight, &plan, "approved")
        .await
        .unwrap();
    service
        .materialize_split_stack(&preflight, &plan)
        .await
        .unwrap();

    let gh_log = fixture.repo.parent().unwrap().join("split-publish-gh.log");
    let gh_path = fixture.repo.parent().unwrap().join("split-publish-gh.sh");
    fs::write(
        &gh_path,
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "stack" ] && [ "$2" = "link" ]; then exit 0; fi
if [ "$1" = "pr" ] && [ "$2" = "edit" ]; then exit 0; fi
if [ "$1" = "pr" ] && [ "$2" = "list" ]; then
  case "$*" in
    *"--head feature.1_first-hunk "*) printf '[{{"number":41,"url":"https://github.com/example/repo/pull/41","state":"OPEN","isDraft":false,"headRefName":"feature.1_first-hunk","baseRefName":"main"}}]' ;;
    *"--head feature.2_second-hunk "*) printf '[{{"number":42,"url":"https://github.com/example/repo/pull/42","state":"OPEN","isDraft":false,"headRefName":"feature.2_second-hunk","baseRefName":"feature.1_first-hunk"}}]' ;;
    *"--head feature "*) printf '[{{"number":43,"url":"https://github.com/example/repo/pull/43","state":"OPEN","isDraft":false,"headRefName":"feature","baseRefName":"feature.2_second-hunk"}}]' ;;
  esac
  exit 0
fi
exit 1
"#,
            log = gh_log.display()
        ),
    )
    .unwrap();
    make_executable(&gh_path);
    let publishing_service =
        DashboardService::new(fixture.repo.clone(), DashboardConfig::default())
            .with_gh_binary(gh_path);
    let publication = publishing_service
        .publish_split_stack(&preflight, &plan)
        .await
        .unwrap();
    assert_eq!(publication.pull_requests.len(), 3);
    assert!(publication
        .pull_requests
        .iter()
        .all(|pull_request| pull_request.provisional_title_applied));
    publishing_service
        .publish_split_stack(&preflight, &plan)
        .await
        .unwrap();
    let gh_calls = fs::read_to_string(&gh_log).unwrap();
    assert_eq!(
        gh_calls
            .lines()
            .filter(|line| line.starts_with("stack link "))
            .collect::<Vec<_>>(),
        ["stack link --base main --open feature.1_first-hunk feature.2_second-hunk feature"]
    );
    assert_eq!(
        gh_calls
            .lines()
            .filter(|line| line.starts_with("pr edit "))
            .count(),
        3
    );

    // A barrier inside the fake provider proves that all three captured jobs
    // are launched before the service awaits any one result. A serialized
    // implementation cannot get past this barrier.
    let barrier = fixture.repo.parent().unwrap().join("split-draft-started");
    fs::create_dir_all(&barrier).unwrap();
    let ai_log = fixture.repo.parent().unwrap().join("split-draft-ai.log");
    let ai_path = fixture.repo.parent().unwrap().join("split-draft-ai.sh");
    fs::write(
        &ai_path,
        format!(
            r#"#!/bin/sh
if [ "$1" = "--version" ]; then printf 'opencode 1.0\n'; exit 0; fi
printf '%s\n' "$PWD" >> "{log}"
touch "{barrier}/$$"
i=0
while [ "$(find "{barrier}" -type f | wc -l | tr -d ' ')" -lt 3 ]; do
  i=$((i + 1))
  if [ "$i" -gt 200 ]; then printf 'drafts were serialized' >&2; exit 1; fi
  sleep 0.01
done
printf '%s\n' '{{"title_summary":"Focused layer metadata","description_content":"Explains this layer responsibility and its focused test guidance."}}'
"#,
            log = ai_log.display(),
            barrier = barrier.display(),
        ),
    )
    .unwrap();
    make_executable(&ai_path);
    let metadata_gh_log = fixture.repo.parent().unwrap().join("split-metadata-gh.log");
    let metadata_gh = fixture.repo.parent().unwrap().join("split-metadata-gh.sh");
    fs::write(
        &metadata_gh,
        format!(
            r##"#!/bin/sh
printf '%s\n' "$*" >> "{log}"
if [ "$1" = "pr" ] && [ "$2" = "view" ]; then printf '%s' '{{"title":"provisional","body":"# Overview 🔍\n\n![old](https://github.com/example/repo/assets/old)"}}'; exit 0; fi
if [ "$1" = "pr" ] && [ "$2" = "edit" ]; then exit 0; fi
exit 1
"##,
            log = metadata_gh_log.display(),
        ),
    )
    .unwrap();
    make_executable(&metadata_gh);
    let drafting_service = DashboardService::new(fixture.repo.clone(), DashboardConfig::default())
        .with_ai_binary(wisetree::config::schema::AiHarness::OpenCode, ai_path)
        .with_gh_binary(metadata_gh);
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let (_cancel_tx, cancel_rx) = oneshot::channel();
    let result = drafting_service
        .draft_and_apply_split_metadata(
            &preflight,
            &plan,
            &publication,
            17,
            4,
            progress_tx,
            cancel_rx,
        )
        .await;
    if let Err(error) = &result {
        let cache = fs::read_dir(fixture.source.join(".wisetree/split_drafts"))
            .unwrap()
            .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect::<Vec<_>>();
        panic!(
            "{error}; ai log={:?}; barrier={:?}; cache={cache:?}",
            fs::read_to_string(&ai_log),
            fs::read_dir(&barrier)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>()
        );
    }
    let records = result.unwrap();
    assert_eq!(records.len(), 3);
    assert!(records.iter().all(|record| record.applied));
    assert_eq!(fs::read_to_string(&ai_log).unwrap().lines().count(), 3);
    let progress = std::iter::from_fn(|| progress_rx.try_recv().ok()).collect::<Vec<_>>();
    assert!(progress.iter().all(|event| event.operation_id == 17));
    assert!(progress.iter().all(|event| event.generation == 4));
    assert!(progress
        .iter()
        .any(|event| event.layer == 3 && event.pr_number == 43));
    let metadata_calls = fs::read_to_string(metadata_gh_log).unwrap();
    assert_eq!(
        metadata_calls
            .lines()
            .filter(|line| line.starts_with("pr edit "))
            .count(),
        3
    );

    // Valid drafts and successful mutations are stable cache hits.
    let (progress_tx, _progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let (_cancel_tx, cancel_rx) = oneshot::channel();
    drafting_service
        .draft_and_apply_split_metadata(
            &preflight,
            &plan,
            &publication,
            17,
            5,
            progress_tx,
            cancel_rx,
        )
        .await
        .unwrap();
    assert_eq!(fs::read_to_string(&ai_log).unwrap().lines().count(), 3);

    assert_eq!(
        git_stdout(&fixture.source, &["rev-parse", "HEAD"]),
        fixture.head
    );
    assert_eq!(
        fs::read(fixture.source.join("tests/shared.txt")).unwrap(),
        source_before
    );
    assert!(git_stdout(&fixture.source, &["diff", "--cached"]).is_empty());
    assert!(git_stdout(&fixture.source, &["diff"]).is_empty());
    let lower_path = fixture
        .repo
        .parent()
        .unwrap()
        .join("repo.worktree")
        .join("feature.1_first-hunk");
    assert_eq!(
        fs::read(lower_path.join("tests/shared.txt")).unwrap(),
        b"ONE\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\n"
    );
    let second_path = fixture
        .repo
        .parent()
        .unwrap()
        .join("repo.worktree")
        .join("feature.2_second-hunk");
    assert_eq!(
        fs::read(second_path.join("tests/shared.txt")).unwrap(),
        b"ONE\ntwo\nthree\nfour\nFIVE\nsix\nseven\neight\nnine\n"
    );
    let document = fs::read_to_string(fixture.source.join(".wisetree/split_plan.md")).unwrap();
    let persisted = parse_materialization(&document).unwrap().unwrap();
    let published = parse_publication(&document).unwrap().unwrap();
    assert_eq!(
        published.pull_requests[2].expected_base,
        "feature.2_second-hunk"
    );
    assert_eq!(published.pull_requests[2].number, 43);
    assert_eq!(
        published.pull_requests[2].url,
        "https://github.com/example/repo/pull/43"
    );
    assert_eq!(persisted.layers.len(), 3);
    assert_eq!(persisted.layers[0].branch, "feature.1_first-hunk");
    assert!(persisted.layers[0].ready_for_publication);

    if let Some(value) = previous_home {
        std::env::set_var("HOME", value);
    } else {
        std::env::remove_var("HOME");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn materializes_deletion_rename_and_binary_and_rejects_moved_artifacts() {
    let _guard = HOME_LOCK.lock().await;
    let home = tempfile::tempdir().unwrap();
    let previous_home = std::env::var_os("HOME");
    std::env::set_var("HOME", home.path());

    let fixture = split_repo(
        &[
            ("tests/delete.txt", b"remove me\n"),
            ("tests/old-name.txt", b"rename me\n"),
            ("tests/blob.bin", &[0, 1, 2, 3]),
            ("tests/top.txt", b"before\n"),
        ],
        &[
            ("tests/delete.txt", None),
            ("tests/old-name.txt", None),
            ("tests/new-name.txt", Some(b"rename me\n")),
            ("tests/blob.bin", Some(&[0, 9, 2, 3])),
            ("tests/top.txt", Some(b"after\n")),
        ],
    );
    fs::write(fixture.repo.join(".env"), "SECRET=kept-out\n").unwrap();
    let range = format!("{}..{}", fixture.base, fixture.head);
    let diff = git_stdout(
        &fixture.source,
        &[
            "diff",
            "--binary",
            "--full-index",
            "--find-renames",
            "--unified=0",
            "--no-color",
            "--no-ext-diff",
            &range,
        ],
    );
    let units = inventory_diff(&diff).unwrap();
    assert!(units.iter().any(|unit| unit.kind == ChangeUnitKind::Binary));
    assert!(units.iter().any(|unit| unit.kind == ChangeUnitKind::Rename));
    assert!(units
        .iter()
        .any(|unit| { unit.kind == ChangeUnitKind::TextHunk && unit.new_lines == Some(0) }));
    let (additions, deletions) = units.iter().fold((0, 0), |counts, unit| {
        (counts.0 + unit.additions, counts.1 + unit.deletions)
    });
    let plan = SplitPlan {
        responsibilities: units
            .iter()
            .enumerate()
            .map(|(index, unit)| SplitResponsibility {
                order: index + 1,
                name: format!("Responsibility {}", index + 1),
                branch_slug: format!("responsibility-{}", index + 1),
                rationale: "Independent test responsibility.".into(),
                units: vec![unit.id.clone()],
                test_units: vec![unit.id.clone()],
                paths: vec![unit.path.clone()],
            })
            .collect(),
    };
    let preflight = SplitPreflight {
        worktree_path: fixture.source.to_string_lossy().into_owned(),
        identity: SplitIdentity {
            repository: "example/repo".into(),
            remote: "origin".into(),
            base_ref: "origin/main".into(),
            base_sha: fixture.base.clone(),
            source_branch: "feature".into(),
            source_head: fixture.head.clone(),
            max: 10,
            additions,
            deletions,
        },
        units,
    };
    parse_split_plan(&serde_json::to_string(&plan).unwrap(), &preflight).unwrap();
    let service = DashboardService::new(fixture.repo.clone(), DashboardConfig::default());
    service
        .save_split_plan(&preflight, &plan, "approved")
        .await
        .unwrap();
    service
        .materialize_split_stack(&preflight, &plan)
        .await
        .unwrap();
    // A second pass must reuse the exact recorded identities without adding commits.
    service
        .materialize_split_stack(&preflight, &plan)
        .await
        .unwrap();

    let document = fs::read_to_string(fixture.source.join(".wisetree/split_plan.md")).unwrap();
    let persisted = parse_materialization(&document).unwrap().unwrap();
    assert_eq!(persisted.layers.len(), plan.responsibilities.len());
    for layer in persisted.layers.iter().take(persisted.layers.len() - 1) {
        assert!(Path::new(&layer.worktree_path).join(".env").exists());
        assert!(git_stdout(
            Path::new(&layer.worktree_path),
            &["ls-tree", "--name-only", "HEAD", ".env"]
        )
        .is_empty());
    }
    assert_eq!(
        git_stdout(&fixture.source, &["rev-parse", "HEAD"]),
        fixture.head
    );
    assert!(git_stdout(&fixture.source, &["diff"]).is_empty());
    assert!(git_stdout(&fixture.source, &["diff", "--cached"]).is_empty());

    let moved = &persisted.layers[persisted.layers.len() - 2];
    fs::write(
        Path::new(&moved.worktree_path).join("unrelated.txt"),
        "moved\n",
    )
    .unwrap();
    git(Path::new(&moved.worktree_path), &["add", "unrelated.txt"]);
    git(
        Path::new(&moved.worktree_path),
        &["commit", "-q", "-m", "unexpected"],
    );
    let error = service
        .materialize_split_stack(&preflight, &plan)
        .await
        .expect_err("moved recorded branch must stop")
        .to_string();
    assert!(
        error.contains("recorded branch or worktree moved"),
        "{error}"
    );

    if let Some(value) = previous_home {
        std::env::set_var("HOME", value);
    } else {
        std::env::remove_var("HOME");
    }
}

/// A drafting failure is scoped to one pull request. The cache key is per
/// source-head + layer + PR, so a retry reuses every valid cached draft (no
/// second AI call, no second `gh pr edit`) and re-runs only the incomplete
/// job — including the case where drafting succeeded but applying did not.
#[test]
fn draft_cache_identity_scopes_a_retry_to_the_incomplete_pull_request() {
    let head = "0123456789abcdef0123456789abcdef01234567";
    let first = split_draft_job_id(head, 1, 91);
    let second = split_draft_job_id(head, 2, 92);
    assert_ne!(first, second);
    assert!(
        first.contains("layer-1") && first.contains("pr-91"),
        "{first}"
    );
    // A different source head can never reuse another run's drafts.
    assert_ne!(split_draft_job_id("f".repeat(40).as_str(), 1, 91), first);

    let path = split_draft_cache_path("/tmp/worktree", &first);
    assert!(
        path.ends_with(format!(".wisetree/split_drafts/{first}.json")),
        "{path:?}"
    );
    assert_ne!(path, split_draft_cache_path("/tmp/worktree", &second));

    // A draft that succeeded but failed to apply keeps its prose, so the retry
    // costs no tokens — only the GitHub update is repeated.
    let applied_but_failed = SplitDraftRecord {
        job_id: second.clone(),
        source_head: head.into(),
        order: 2,
        pr_number: 92,
        pr_url: "https://github.com/owner/repo/pull/92".into(),
        correction_attempted: false,
        draft: Some(SplitDraft {
            title_summary: "Add the consumer".into(),
            description_content: "Details".into(),
        }),
        final_title: Some("Add the consumer (2/2)".into()),
        final_body: Some("body".into()),
        applied: false,
        error: Some("gh pr edit: server error".into()),
    };
    let round_tripped: SplitDraftRecord =
        serde_json::from_str(&serde_json::to_string(&applied_but_failed).unwrap()).unwrap();
    assert_eq!(round_tripped, applied_but_failed);
    assert!(round_tripped.draft.is_some() && !round_tripped.applied);
}
