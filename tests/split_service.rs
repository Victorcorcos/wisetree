use wisetree::services::{
    build_split_plan_prompt, inventory_diff, parse_numstat_totals, parse_split_plan,
    render_split_plan, validate_split_manifest, ChangeUnit, ChangeUnitKind, SplitIdentity,
    SplitPreflight,
};

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

#[test]
fn indivisible_unit_over_max_names_the_exact_unit_and_path() {
    let mut preflight = fixture();
    preflight.identity.max = 3;
    preflight.units[0].kind = ChangeUnitKind::Binary;
    let error = validate_split_manifest(&preflight.identity, &preflight.units)
        .expect_err("oversized atomic unit")
        .to_string();
    assert!(error.contains("CU0001"), "{error}");
    assert!(error.contains("src/a.rs"), "{error}");
    assert!(error.contains("No valid split"), "{error}");
}

#[test]
fn prompt_contains_frozen_manifest_and_revision_only_when_both_inputs_exist() {
    let preflight = fixture();
    let first = build_split_plan_prompt(&preflight, None, None);
    assert!(first.contains("origin/main"));
    assert!(first.contains("CU0001 | TextHunk | src/a.rs | +3 -1"));
    assert!(first.contains("Do not write files"));
    let incomplete_revision = build_split_plan_prompt(&preflight, Some("stale"), None);
    assert!(!incomplete_revision.contains("stale"));

    let revision = build_split_plan_prompt(&preflight, Some(valid_response()), Some("move B"));
    assert!(revision.contains(valid_response()));
    assert!(revision.contains("move B"));
}
