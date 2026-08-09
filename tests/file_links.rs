use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use tempfile::TempDir;
use wisetree::config::{LinkStrategy, WorktreeConfig};
use wisetree::files::{
    clear_cache, link_patterns, list_cache, prune_cache, remove_cache_entry,
    touch_worktree_entry_last_seen, unlink_patterns, unregister_worktree_user,
};

fn dirs() -> (
    TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let root = tempfile::tempdir().expect("tempdir");
    let source = root.path().join("source");
    let target = root.path().join("target");
    let cache = root.path().join("cache");
    fs::create_dir_all(&source).unwrap();
    fs::create_dir_all(&target).unwrap();
    (root, source, target, cache)
}

fn link_config(strategy: LinkStrategy) -> WorktreeConfig {
    WorktreeConfig {
        worktree_copy_patterns: Vec::new(),
        worktree_link_patterns: vec!["node_modules".into()],
        worktree_link_strategy: strategy,
        ..WorktreeConfig::default()
    }
}

fn is_link(path: &Path) -> bool {
    let metadata = fs::symlink_metadata(path).expect("metadata");
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_type().is_symlink()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[tokio::test]
async fn create_empty_links_into_cache() {
    let (_root, source, target, cache) = dirs();
    let report = link_patterns(
        &source,
        &target,
        &cache,
        &link_config(LinkStrategy::CreateEmpty),
    )
    .await;

    assert_eq!(report.linked.len(), 1);
    assert!(cache.join("entries/node_modules").is_dir());
    assert!(is_link(&target.join("node_modules")));
}

#[tokio::test]
async fn linking_is_idempotent() {
    let (_root, source, target, cache) = dirs();
    let config = link_config(LinkStrategy::CreateEmpty);

    let first = link_patterns(&source, &target, &cache, &config).await;
    let second = link_patterns(&source, &target, &cache, &config).await;

    assert_eq!(first.linked.len(), 1);
    assert!(second.linked.is_empty());
    assert!(second.errors.is_empty(), "{:?}", second.errors);
    assert!(second
        .skipped
        .iter()
        .any(|item| item.contains("already linked")));
}

#[tokio::test]
async fn existing_real_directory_is_left_intact() {
    let (_root, source, target, cache) = dirs();
    fs::create_dir_all(target.join("node_modules")).unwrap();
    fs::write(target.join("node_modules/keep.txt"), "keep").unwrap();

    let report = link_patterns(
        &source,
        &target,
        &cache,
        &link_config(LinkStrategy::CreateEmpty),
    )
    .await;

    assert!(!report.errors.is_empty());
    assert!(target.join("node_modules/keep.txt").exists());
    assert!(!is_link(&target.join("node_modules")));
}

#[tokio::test]
async fn seed_from_source_only_copies_once() {
    let (_root, source, target, cache) = dirs();
    fs::create_dir_all(source.join("node_modules/pkg")).unwrap();
    fs::write(source.join("node_modules/pkg/index.js"), "v1").unwrap();
    let config = link_config(LinkStrategy::SeedFromSource);

    let first = link_patterns(&source, &target, &cache, &config).await;
    fs::write(source.join("node_modules/pkg/index.js"), "v2").unwrap();
    let second = link_patterns(&source, &target, &cache, &config).await;

    assert!(first.linked[0].seeded);
    assert!(second.linked.is_empty());
    assert_eq!(
        fs::read_to_string(cache.join("entries/node_modules/pkg/index.js")).unwrap(),
        "v1"
    );
}

#[tokio::test]
async fn unlink_removes_only_links() {
    let (_root, source, target, cache) = dirs();
    let config = link_config(LinkStrategy::CreateEmpty);

    let _ = link_patterns(&source, &target, &cache, &config).await;
    unlink_patterns(&target, &config).await.expect("unlink ok");
    assert!(!target.join("node_modules").exists());

    fs::create_dir_all(target.join("node_modules")).unwrap();
    unlink_patterns(&target, &config)
        .await
        .expect("unlink real dir ok");
    assert!(target.join("node_modules").exists());
}

#[tokio::test]
async fn invalid_metadata_is_reported_instead_of_being_silently_rewritten() {
    let (_root, source, target, cache) = dirs();
    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join("metadata.json"), "{ not valid json").unwrap();

    let report = link_patterns(
        &source,
        &target,
        &cache,
        &link_config(LinkStrategy::CreateEmpty),
    )
    .await;

    assert!(report
        .errors
        .iter()
        .any(|message| message.contains("Failed to load cache metadata")));
    assert!(!target.join("node_modules").exists());
    assert!(list_cache(&cache).await.is_err());
}

#[tokio::test]
async fn link_patterns_reject_non_directory_sources() {
    let (_root, source, target, cache) = dirs();
    fs::write(source.join("node_modules"), "not a directory").unwrap();

    let report = link_patterns(
        &source,
        &target,
        &cache,
        &link_config(LinkStrategy::CreateEmpty),
    )
    .await;

    assert!(report
        .errors
        .iter()
        .any(|message| message.contains("only directories can be shared")));
    assert!(!target.join("node_modules").exists());
}

#[tokio::test]
async fn prune_is_per_entry_and_uses_last_seen_grace_period() {
    let (_root, source, target, cache) = dirs();
    let report = link_patterns(
        &source,
        &target,
        &cache,
        &link_config(LinkStrategy::CreateEmpty),
    )
    .await;
    assert_eq!(report.linked.len(), 1);

    fs::create_dir_all(cache.join("entries/target")).unwrap();

    let now = now_unix_ms();
    let old = now.saturating_sub(15 * 24 * 60 * 60 * 1000);
    fs::write(
        cache.join("metadata.json"),
        serde_json::to_string_pretty(&json!({
            "gitRoot": source.display().to_string(),
            "createdAt": now,
            "patterns": ["node_modules", "target"],
            "users": [
                {
                    "worktreePath": target.display().to_string(),
                    "lastSeen": now
                }
            ],
            "entryLastSeen": {
                "node_modules": now,
                "target": old
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let overview = list_cache(&cache).await.expect("cache overview");
    let node_modules = overview
        .entries
        .iter()
        .find(|entry| entry.relative_path == "node_modules")
        .expect("node_modules entry");
    let target_entry = overview
        .entries
        .iter()
        .find(|entry| entry.relative_path == "target")
        .expect("target entry");
    assert_eq!(node_modules.user_count, 1);
    assert_eq!(target_entry.user_count, 0);

    let report = prune_cache(&cache).await.expect("prune ok");
    assert!(report.removed.iter().any(|pattern| pattern == "target"));
    assert!(report
        .skipped
        .iter()
        .any(|message| message.contains("node_modules") && message.contains("active worktrees")));
    assert!(cache.join("entries/node_modules").exists());
    assert!(!cache.join("entries/target").exists());
}

#[tokio::test]
async fn parent_dir_patterns_are_rejected_and_cannot_escape_roots() {
    let (root, source, target, cache) = dirs();
    let mut config = link_config(LinkStrategy::CreateEmpty);
    config.worktree_link_patterns = vec!["../escape".into(), "../../etc".into()];

    let report = link_patterns(&source, &target, &cache, &config).await;

    // Nothing should be linked or created anywhere outside `cache/entries` or `target`.
    assert!(report.linked.is_empty(), "linked: {:?}", report.linked);
    // The cleaned patterns are empty strings, so they get filtered out before
    // they ever reach `process_pattern`. Confirm no escape directories exist.
    assert!(!root.path().join("escape").exists());
    assert!(!root.path().join("etc").exists());
    assert!(!cache.join("escape").exists());
    assert!(!target.join("escape").exists());
    // Cache entries directory does not contain anything outside its bounds.
    if cache.join("entries").exists() {
        let entries: Vec<_> = fs::read_dir(cache.join("entries"))
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            entries.is_empty(),
            "unexpected cache entries: {:?}",
            entries.iter().map(|e| e.file_name()).collect::<Vec<_>>()
        );
    }
}

#[tokio::test]
async fn seed_if_present_skips_when_source_missing() {
    let (_root, source, target, cache) = dirs();
    let report = link_patterns(
        &source,
        &target,
        &cache,
        &link_config(LinkStrategy::SeedIfPresent),
    )
    .await;

    assert!(report.linked.is_empty(), "linked: {:?}", report.linked);
    assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
    assert!(report
        .skipped
        .iter()
        .any(|message| message.contains("source directory missing")));
    assert!(!target.join("node_modules").exists());
    assert!(!cache.join("entries/node_modules").exists());
}

#[tokio::test]
async fn seed_if_present_copies_when_source_exists() {
    let (_root, source, target, cache) = dirs();
    fs::create_dir_all(source.join("node_modules/pkg")).unwrap();
    fs::write(source.join("node_modules/pkg/index.js"), "seed").unwrap();

    let report = link_patterns(
        &source,
        &target,
        &cache,
        &link_config(LinkStrategy::SeedIfPresent),
    )
    .await;

    assert_eq!(report.linked.len(), 1);
    assert!(report.linked[0].seeded);
    assert!(is_link(&target.join("node_modules")));
    assert_eq!(
        fs::read_to_string(cache.join("entries/node_modules/pkg/index.js")).unwrap(),
        "seed"
    );
}

#[tokio::test]
async fn literal_pattern_links_only_the_exact_directory() {
    let (_root, source, target, cache) = dirs();
    fs::create_dir_all(source.join("node_modules/package/node_modules/dependency")).unwrap();

    let report = link_patterns(
        &source,
        &target,
        &cache,
        &link_config(LinkStrategy::SeedIfPresent),
    )
    .await;

    assert_eq!(report.linked.len(), 1, "report: {report:?}");
    assert_eq!(report.linked[0].pattern, "node_modules");
    assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
    assert!(is_link(&target.join("node_modules")));
    assert!(cache
        .join("entries/node_modules/package/node_modules/dependency")
        .is_dir());
}

#[tokio::test]
async fn literal_pattern_repairs_legacy_nested_cache_metadata() {
    let (_root, source, target, cache) = dirs();
    let canonical_source = source.canonicalize().unwrap();
    fs::create_dir_all(source.join("node_modules/package/node_modules/dependency")).unwrap();
    fs::create_dir_all(cache.join("entries/node_modules/package/node_modules")).unwrap();
    fs::write(
        cache.join("metadata.json"),
        serde_json::to_string_pretty(&json!({
            "gitRoot": canonical_source.display().to_string(),
            "createdAt": now_unix_ms(),
            "patterns": ["node_modules", "node_modules/package/node_modules"],
            "users": [],
            "entryLastSeen": {
                "node_modules": 1,
                "node_modules/package/node_modules": 1
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let report = link_patterns(
        &source,
        &target,
        &cache,
        &link_config(LinkStrategy::SeedIfPresent),
    )
    .await;
    let metadata: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(cache.join("metadata.json")).unwrap()).unwrap();

    assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
    assert_eq!(metadata["patterns"], json!(["node_modules"]));
    assert_eq!(
        metadata["entryLastSeen"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec!["node_modules"]
    );
}

#[tokio::test]
async fn glob_patterns_match_multiple_directories() {
    let (_root, source, target, cache) = dirs();
    // Create a monorepo-like layout with several node_modules dirs.
    fs::create_dir_all(source.join("apps/web/node_modules")).unwrap();
    fs::create_dir_all(source.join("apps/api/node_modules")).unwrap();
    fs::create_dir_all(target.join("apps/web")).unwrap();
    fs::create_dir_all(target.join("apps/api")).unwrap();

    let config = WorktreeConfig {
        worktree_copy_patterns: Vec::new(),
        worktree_link_patterns: vec!["apps/*/node_modules".into()],
        worktree_link_strategy: LinkStrategy::CreateEmpty,
        ..WorktreeConfig::default()
    };

    let report = link_patterns(&source, &target, &cache, &config).await;

    assert_eq!(report.linked.len(), 2, "report: {report:?}");
    let linked: Vec<_> = report.linked.iter().map(|e| e.pattern.clone()).collect();
    assert!(linked.contains(&"apps/web/node_modules".to_string()));
    assert!(linked.contains(&"apps/api/node_modules".to_string()));
    assert!(is_link(&target.join("apps/web/node_modules")));
    assert!(is_link(&target.join("apps/api/node_modules")));
    assert!(cache.join("entries/apps/web/node_modules").is_dir());
    assert!(cache.join("entries/apps/api/node_modules").is_dir());
}

#[tokio::test]
async fn link_to_different_repo_in_cache_is_rejected() {
    let (root, source, target, cache) = dirs();
    let other_source = root.path().join("other_source");
    fs::create_dir_all(&other_source).unwrap();

    // First, prime the cache from `other_source`.
    let first = link_patterns(
        &other_source,
        &target,
        &cache,
        &link_config(LinkStrategy::CreateEmpty),
    )
    .await;
    assert!(first.errors.is_empty(), "first errors: {:?}", first.errors);

    // Then attempt to use the same cache from a different source (different git root).
    let other_target = root.path().join("other_target");
    fs::create_dir_all(&other_target).unwrap();
    let second = link_patterns(
        &source,
        &other_target,
        &cache,
        &link_config(LinkStrategy::CreateEmpty),
    )
    .await;

    assert!(
        second
            .errors
            .iter()
            .any(|message| message.contains("different repository")),
        "expected repository mismatch error, got: {:?}",
        second.errors
    );
    assert!(!other_target.join("node_modules").exists());
}

#[tokio::test]
async fn stale_symlink_to_wrong_target_is_refused() {
    let (root, source, target, cache) = dirs();
    let bogus_target = root.path().join("bogus");
    fs::create_dir_all(&bogus_target).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&bogus_target, target.join("node_modules")).unwrap();

    let report = link_patterns(
        &source,
        &target,
        &cache,
        &link_config(LinkStrategy::CreateEmpty),
    )
    .await;

    assert!(
        report
            .errors
            .iter()
            .any(|m| m.contains("refusing to replace existing path")),
        "expected refusal error, got: {:?}",
        report.errors
    );
    // The stale link is preserved (we refuse to overwrite).
    let metadata = fs::symlink_metadata(target.join("node_modules")).unwrap();
    assert!(metadata.file_type().is_symlink());
}

#[tokio::test]
async fn touch_last_seen_updates_metadata_timestamp() {
    let (_root, source, target, cache) = dirs();
    let config = link_config(LinkStrategy::CreateEmpty);

    let _ = link_patterns(&source, &target, &cache, &config).await;

    // Force entry_last_seen back in time so we can verify it advances.
    let metadata_path = cache.join("metadata.json");
    let raw = fs::read_to_string(&metadata_path).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    value["entryLastSeen"]["node_modules"] = json!(1u64);
    fs::write(
        &metadata_path,
        serde_json::to_string_pretty(&value).unwrap(),
    )
    .unwrap();

    touch_worktree_entry_last_seen(&cache, &target, &config)
        .await
        .expect("touch ok");

    let raw_after = fs::read_to_string(&metadata_path).unwrap();
    let value_after: serde_json::Value = serde_json::from_str(&raw_after).unwrap();
    let new_ts = value_after["entryLastSeen"]["node_modules"]
        .as_u64()
        .unwrap();
    assert!(new_ts > 1, "expected timestamp to advance, got {new_ts}");
}

#[tokio::test]
async fn unregister_worktree_user_removes_user_from_metadata() {
    let (_root, source, target, cache) = dirs();
    let config = link_config(LinkStrategy::CreateEmpty);

    let _ = link_patterns(&source, &target, &cache, &config).await;

    let overview = list_cache(&cache).await.expect("overview");
    assert!(overview
        .users
        .iter()
        .any(|u| Path::new(&u.worktree_path) == target));

    unregister_worktree_user(&cache, &target)
        .await
        .expect("unregister ok");

    let overview = list_cache(&cache).await.expect("overview after");
    assert!(!overview
        .users
        .iter()
        .any(|u| Path::new(&u.worktree_path) == target));
}

#[tokio::test]
async fn remove_cache_entry_drops_directory_and_metadata() {
    let (_root, source, target, cache) = dirs();
    let _ = link_patterns(
        &source,
        &target,
        &cache,
        &link_config(LinkStrategy::CreateEmpty),
    )
    .await;

    assert!(cache.join("entries/node_modules").is_dir());

    remove_cache_entry(&cache, "node_modules")
        .await
        .expect("remove ok");

    assert!(!cache.join("entries/node_modules").exists());
    let raw = fs::read_to_string(cache.join("metadata.json")).unwrap();
    let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let patterns = value["patterns"].as_array().unwrap();
    assert!(patterns.iter().all(|p| p.as_str() != Some("node_modules")));
}

#[tokio::test]
async fn remove_cache_entry_rejects_invalid_traversal_pattern() {
    let (_root, _source, _target, cache) = dirs();
    fs::create_dir_all(cache.join("entries/keep")).unwrap();

    let err = remove_cache_entry(&cache, "../entries")
        .await
        .expect_err("must reject traversal");
    assert!(
        err.to_string().to_lowercase().contains("invalid"),
        "unexpected error: {err}"
    );
    // The legitimate cache entry survives the rejected call.
    assert!(cache.join("entries/keep").exists());
}

#[tokio::test]
async fn clear_cache_removes_the_repo_cache_dir() {
    let (_root, source, target, cache) = dirs();
    let _ = link_patterns(
        &source,
        &target,
        &cache,
        &link_config(LinkStrategy::CreateEmpty),
    )
    .await;

    assert!(cache.exists());
    clear_cache(&cache).await.expect("clear ok");
    assert!(!cache.exists());
}

#[tokio::test]
async fn user_count_reflects_multiple_active_worktrees() {
    let (root, source, _target, cache) = dirs();
    let target_a = root.path().join("target_a");
    let target_b = root.path().join("target_b");
    fs::create_dir_all(&target_a).unwrap();
    fs::create_dir_all(&target_b).unwrap();

    let config = link_config(LinkStrategy::CreateEmpty);
    let _ = link_patterns(&source, &target_a, &cache, &config).await;
    let _ = link_patterns(&source, &target_b, &cache, &config).await;

    let overview = list_cache(&cache).await.expect("overview");
    let entry = overview
        .entries
        .iter()
        .find(|e| e.relative_path == "node_modules")
        .expect("node_modules entry");
    assert_eq!(entry.user_count, 2, "entry users: {:?}", entry.users);
}

#[tokio::test]
async fn list_cache_drops_users_whose_worktrees_no_longer_exist() {
    let (root, source, _target, cache) = dirs();
    let target_a = root.path().join("target_a");
    let target_b = root.path().join("target_b");
    fs::create_dir_all(&target_a).unwrap();
    fs::create_dir_all(&target_b).unwrap();

    let config = link_config(LinkStrategy::CreateEmpty);
    let _ = link_patterns(&source, &target_a, &cache, &config).await;
    let _ = link_patterns(&source, &target_b, &cache, &config).await;

    // Simulate a deleted worktree on disk without going through unregister.
    fs::remove_dir_all(&target_b).unwrap();

    let overview = list_cache(&cache).await.expect("overview");
    assert!(overview
        .users
        .iter()
        .all(|u| Path::new(&u.worktree_path).exists()));
    assert_eq!(overview.users.len(), 1);
}

#[tokio::test]
async fn unlink_then_relink_reuses_existing_cache_entry() {
    let (_root, source, target, cache) = dirs();
    fs::create_dir_all(source.join("node_modules/pkg")).unwrap();
    fs::write(source.join("node_modules/pkg/index.js"), "v1").unwrap();
    let config = link_config(LinkStrategy::SeedFromSource);

    let first = link_patterns(&source, &target, &cache, &config).await;
    assert!(first.linked[0].seeded);

    unlink_patterns(&target, &config).await.expect("unlink ok");
    assert!(!target.join("node_modules").exists());

    // Even if the source changes, the cache content is preserved.
    fs::write(source.join("node_modules/pkg/index.js"), "v2").unwrap();
    let second = link_patterns(&source, &target, &cache, &config).await;
    assert_eq!(second.linked.len(), 1);
    assert!(
        !second.linked[0].seeded,
        "should reuse existing cache, not reseed"
    );
    assert_eq!(
        fs::read_to_string(cache.join("entries/node_modules/pkg/index.js")).unwrap(),
        "v1"
    );
}
