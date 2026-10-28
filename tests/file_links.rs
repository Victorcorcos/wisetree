use std::{fs, path::Path};
use wisetree::config::WorktreeConfig;
use wisetree::files::{copy_files, link_patterns, unlink_patterns};

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let mother = root.path().join("mother");
    let child = root.path().join("child");
    fs::create_dir_all(&mother).unwrap();
    fs::create_dir_all(&child).unwrap();
    (root, mother, child)
}
fn config(patterns: &[&str]) -> WorktreeConfig {
    WorktreeConfig {
        worktree_link_patterns: patterns.iter().map(|p| (*p).into()).collect(),
        ..Default::default()
    }
}
fn write(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

#[tokio::test]
async fn files_and_folders_share_mother_and_sibling_edits_in_both_directions() {
    let (_root, mother, child) = fixture();
    let sibling = child.with_file_name("sibling");
    fs::create_dir_all(&sibling).unwrap();
    let config = config(&[".env", "node_modules"]);
    for relative in [".env", "node_modules/pkg/index.js"] {
        write(&mother, relative, "initial");
    }
    for target in [&child, &sibling] {
        let report = link_patterns(&mother, target, &config).await;
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        assert_eq!(report.linked.len(), 2);
        for entry in report.linked {
            assert!(fs::symlink_metadata(&entry.link_path)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(
                fs::read_link(entry.link_path).unwrap(),
                mother.join(entry.pattern).canonicalize().unwrap()
            );
        }
    }
    for relative in [".env", "node_modules/pkg/index.js"] {
        write(&child, relative, "child edit");
        assert_eq!(
            fs::read_to_string(mother.join(relative)).unwrap(),
            "child edit"
        );
        assert_eq!(
            fs::read_to_string(sibling.join(relative)).unwrap(),
            "child edit"
        );
        write(&mother, relative, "mother edit");
        assert_eq!(
            fs::read_to_string(child.join(relative)).unwrap(),
            "mother edit"
        );
        assert_eq!(
            fs::read_to_string(sibling.join(relative)).unwrap(),
            "mother edit"
        );
    }
    write(&child, "node_modules/new.js", "installed");
    assert!(mother.join("node_modules/new.js").exists());
    fs::remove_file(sibling.join("node_modules/new.js")).unwrap();
    assert!(!mother.join("node_modules/new.js").exists());
    unlink_patterns(&child, &config).await.unwrap();
    assert!(mother.join("node_modules/pkg/index.js").is_file());
    assert!(mother.join(".env").is_file());
    assert!(fs::symlink_metadata(child.join("node_modules")).is_err());
}

#[tokio::test]
async fn globs_share_nested_files_and_folders_and_win_over_recursive_copies() {
    let (_root, mother, child) = fixture();
    for relative in [
        ".env",
        "apps/api/.env.local",
        "config/application.yml",
        "apps/api/config/application.yml",
        "node_modules/pkg/index.js",
        "apps/api/node_modules/pkg/index.js",
        "apps/api/keep.txt",
    ] {
        write(&mother, relative, "original");
    }
    let mut config = config(&[
        ".env",
        "**/.env*",
        "**/config/application.yml",
        "**/node_modules",
    ]);
    config.worktree_copy_patterns = vec![".env*".into(), "apps".into()];
    let copied = copy_files(&mother, &child, &config).await;
    assert!(copied.errors.is_empty(), "{:?}", copied.errors);
    assert_eq!(copied.copied, vec!["apps/api/keep.txt"]);
    let linked = link_patterns(&mother, &child, &config).await;
    assert!(linked.errors.is_empty(), "{:?}", linked.errors);
    assert_eq!(linked.linked.len(), 6);
    unlink_patterns(&child, &config).await.unwrap();
    assert!(mother.join("apps/api/config/application.yml").is_file());
    assert!(mother.join("apps/api/node_modules/pkg/index.js").is_file());
    assert!(!child.join("apps/api/.env.local").exists());
}

#[tokio::test]
async fn missing_sources_are_skipped_and_unsafe_paths_cannot_escape_roots() {
    let (root, mother, child) = fixture();
    write(root.path(), "outside", "keep");
    let report = link_patterns(
        &mother,
        &child,
        &config(&[
            ".env",
            "node_modules",
            "../outside",
            "/outside",
            "nested/../../outside",
        ]),
    )
    .await;
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(report.linked.is_empty());
    assert_eq!(report.skipped.len(), 2);
    assert_eq!(fs::read_dir(&child).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&mother).unwrap().count(), 0);
    assert_eq!(
        fs::read_to_string(root.path().join("outside")).unwrap(),
        "keep"
    );
}

#[tokio::test]
async fn overlapping_patterns_are_idempotent_and_existing_paths_are_preserved() {
    let (_root, mother, child) = fixture();
    write(&mother, "node_modules/pkg/index.js", "keep");
    write(&mother, ".env", "secret");
    let config = config(&[".env", "node_modules", "node_modules/**"]);
    assert_eq!(
        link_patterns(&mother, &child, &config).await.linked.len(),
        2
    );
    let repeated = link_patterns(&mother, &child, &config).await;
    assert!(repeated.errors.is_empty(), "{:?}", repeated.errors);
    assert_eq!(repeated.skipped.len(), 2);
    assert!(!fs::symlink_metadata(mother.join("node_modules/pkg"))
        .unwrap()
        .file_type()
        .is_symlink());
    unlink_patterns(&child, &config).await.unwrap();
    write(&child, ".env", "keep file");
    write(&child, "node_modules/keep", "keep folder");
    assert_eq!(
        link_patterns(&mother, &child, &config).await.errors.len(),
        2
    );
    unlink_patterns(&child, &config).await.unwrap();
    assert_eq!(fs::read_to_string(child.join(".env")).unwrap(), "keep file");
    assert_eq!(
        fs::read_to_string(child.join("node_modules/keep")).unwrap(),
        "keep folder"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn source_links_and_unrelated_destination_links_are_not_followed_or_replaced() {
    let (root, mother, child) = fixture();
    write(root.path(), "outside", "keep");
    std::os::unix::fs::symlink(root.path().join("outside"), mother.join(".env")).unwrap();
    let config = config(&[".env"]);
    assert_eq!(
        link_patterns(&mother, &child, &config).await.errors.len(),
        1
    );
    fs::remove_file(mother.join(".env")).unwrap();
    write(&mother, ".env", "original");
    std::os::unix::fs::symlink(root.path().join("outside"), child.join(".env")).unwrap();
    assert_eq!(
        link_patterns(&mother, &child, &config).await.errors.len(),
        1
    );
    assert_eq!(fs::read_to_string(child.join(".env")).unwrap(), "keep");
}
