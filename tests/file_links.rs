use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;
use tempfile::TempDir;
use wisetree::config::{LinkStrategy, WorktreeConfig};
use wisetree::files::{link_patterns, list_cache, prune_cache, unlink_patterns};

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
