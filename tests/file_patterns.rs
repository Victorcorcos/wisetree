use std::fs;

use wisetree::files::patterns::{match_files, normalize_patterns, should_ignore_file};

#[test]
fn normalize_adds_globstar_prefix_for_bare_patterns() {
    let out = normalize_patterns(&[".env*".to_string(), "**/dist/**".to_string()]);
    assert!(out.contains(&".env*".to_string()));
    assert!(out.contains(&"**/.env*".to_string()));
    assert!(out.contains(&"**/dist/**".to_string()));
}

#[test]
fn normalize_skips_absolute_and_globstar() {
    let out = normalize_patterns(&["/abs".to_string(), "**/already".to_string()]);
    assert!(out.contains(&"/abs".to_string()));
    assert!(out.contains(&"**/already".to_string()));
    assert!(!out.contains(&"**//abs".to_string()));
    assert!(!out.contains(&"**/**/already".to_string()));
}

#[test]
fn normalize_drops_empty_strings() {
    let out = normalize_patterns(&[String::new(), "x".to_string()]);
    assert_eq!(out.len(), 2);
    assert!(out.contains(&"x".to_string()));
    assert!(out.contains(&"**/x".to_string()));
}

#[test]
fn match_files_uses_default_config() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let base = tmp.path();

    fs::write(base.join(".env"), "X=1").unwrap();
    fs::write(base.join(".env.local"), "Y=2").unwrap();
    fs::create_dir_all(base.join(".vscode")).unwrap();
    fs::write(base.join(".vscode/settings.json"), "{}").unwrap();
    fs::create_dir_all(base.join("node_modules/foo")).unwrap();
    fs::write(base.join("node_modules/foo/package.json"), "{}").unwrap();
    fs::write(base.join("README.md"), "hi").unwrap();

    let patterns = vec![".env*".to_string(), ".vscode/**".to_string()];
    let ignores = vec!["**/node_modules/**".to_string()];
    let matched = match_files(base, &patterns, &ignores);

    assert!(matched.iter().any(|p| p == ".env"));
    assert!(matched.iter().any(|p| p == ".env.local"));
    assert!(matched.iter().any(|p| p == ".vscode/settings.json"));
    assert!(!matched.iter().any(|p| p.contains("node_modules")));
    assert!(!matched.iter().any(|p| p == "README.md"));
}

#[test]
fn should_ignore_matches_globstar() {
    assert!(should_ignore_file(
        "node_modules/foo/index.js",
        &["**/node_modules/**".into()]
    ));
    assert!(!should_ignore_file(
        "src/foo.ts",
        &["**/node_modules/**".into()]
    ));
}
