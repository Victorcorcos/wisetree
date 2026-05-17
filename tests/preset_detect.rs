//! Fixture-based detection tests for the project setup presets.

use std::fs;
use std::path::Path;

use tempfile::TempDir;
use wisetree::services::presets::{catalog, detect, PresetId};

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn touch(root: &Path, rel: &str) {
    write(root, rel, "");
}

fn fixture() -> TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn detects_ruby_on_rails() {
    let dir = fixture();
    touch(dir.path(), "Gemfile");
    touch(dir.path(), "config/application.rb");
    assert_eq!(detect(dir.path()), Some(PresetId::RubyOnRails));
}

#[test]
fn detects_django() {
    let dir = fixture();
    touch(dir.path(), "manage.py");
    touch(dir.path(), "requirements.txt");
    assert_eq!(detect(dir.path()), Some(PresetId::Django));
}

#[test]
fn detects_fastapi_before_generic_python() {
    let dir = fixture();
    write(
        dir.path(),
        "pyproject.toml",
        "[project]\nname = \"x\"\ndependencies = [\"fastapi\"]\n",
    );
    assert_eq!(detect(dir.path()), Some(PresetId::FastApi));
}

#[test]
fn detects_flask() {
    let dir = fixture();
    write(dir.path(), "requirements.txt", "Flask==3.0\n");
    assert_eq!(detect(dir.path()), Some(PresetId::Flask));
}

#[test]
fn detects_next_over_plain_react() {
    let dir = fixture();
    write(
        dir.path(),
        "package.json",
        "{\"dependencies\": {\"react\": \"18\", \"next\": \"14\"}}",
    );
    touch(dir.path(), "next.config.js");
    assert_eq!(detect(dir.path()), Some(PresetId::NextJs));
}

#[test]
fn detects_react_when_no_meta_framework() {
    let dir = fixture();
    write(
        dir.path(),
        "package.json",
        "{\"dependencies\": {\"react\": \"18\"}}",
    );
    assert_eq!(detect(dir.path()), Some(PresetId::React));
}

#[test]
fn detects_remix() {
    let dir = fixture();
    write(dir.path(), "package.json", "{}");
    touch(dir.path(), "remix.config.js");
    assert_eq!(detect(dir.path()), Some(PresetId::Remix));
}

#[test]
fn detects_nestjs() {
    let dir = fixture();
    touch(dir.path(), "nest-cli.json");
    assert_eq!(detect(dir.path()), Some(PresetId::NestJs));
}

#[test]
fn detects_nuxt() {
    let dir = fixture();
    touch(dir.path(), "nuxt.config.ts");
    assert_eq!(detect(dir.path()), Some(PresetId::VueNuxt));
}

#[test]
fn detects_angular() {
    let dir = fixture();
    touch(dir.path(), "angular.json");
    assert_eq!(detect(dir.path()), Some(PresetId::Angular));
}

#[test]
fn detects_svelte() {
    let dir = fixture();
    touch(dir.path(), "svelte.config.js");
    assert_eq!(detect(dir.path()), Some(PresetId::Svelte));
}

#[test]
fn detects_astro() {
    let dir = fixture();
    touch(dir.path(), "astro.config.mjs");
    assert_eq!(detect(dir.path()), Some(PresetId::Astro));
}

#[test]
fn detects_plain_node_as_express() {
    let dir = fixture();
    write(dir.path(), "package.json", "{\"name\":\"x\"}");
    assert_eq!(detect(dir.path()), Some(PresetId::ExpressNode));
}

#[test]
fn detects_flutter() {
    let dir = fixture();
    touch(dir.path(), "pubspec.yaml");
    touch(dir.path(), "lib/main.dart");
    assert_eq!(detect(dir.path()), Some(PresetId::Flutter));
}

#[test]
fn detects_spring_boot_maven() {
    let dir = fixture();
    write(
        dir.path(),
        "pom.xml",
        "<project>spring-boot-starter</project>",
    );
    assert_eq!(detect(dir.path()), Some(PresetId::SpringBootMaven));
}

#[test]
fn detects_spring_boot_gradle() {
    let dir = fixture();
    write(
        dir.path(),
        "build.gradle.kts",
        "plugins { id(\"org.springframework.boot.spring-boot\") }",
    );
    assert_eq!(detect(dir.path()), Some(PresetId::SpringBootGradle));
}

#[test]
fn detects_android() {
    let dir = fixture();
    touch(dir.path(), "settings.gradle");
    touch(dir.path(), "app/build.gradle");
    touch(dir.path(), "app/src/main/AndroidManifest.xml");
    assert_eq!(detect(dir.path()), Some(PresetId::Android));
}

#[test]
fn detects_ios() {
    let dir = fixture();
    fs::create_dir_all(dir.path().join("MyApp.xcodeproj")).unwrap();
    assert_eq!(detect(dir.path()), Some(PresetId::Ios));
}

#[test]
fn detects_dotnet() {
    let dir = fixture();
    touch(dir.path(), "MyApp.csproj");
    assert_eq!(detect(dir.path()), Some(PresetId::DotNet));
}

#[test]
fn detects_go() {
    let dir = fixture();
    touch(dir.path(), "go.mod");
    assert_eq!(detect(dir.path()), Some(PresetId::Go));
}

#[test]
fn detects_rust() {
    let dir = fixture();
    touch(dir.path(), "Cargo.toml");
    assert_eq!(detect(dir.path()), Some(PresetId::Rust));
}

#[test]
fn detects_laravel() {
    let dir = fixture();
    write(
        dir.path(),
        "composer.json",
        "{\"require\": {\"laravel/framework\": \"^11\"}}",
    );
    assert_eq!(detect(dir.path()), Some(PresetId::Laravel));
}

#[test]
fn detects_phoenix() {
    let dir = fixture();
    write(
        dir.path(),
        "mix.exs",
        "defp deps do [{:phoenix, \"~> 1.7\"}] end",
    );
    assert_eq!(detect(dir.path()), Some(PresetId::Phoenix));
}

#[test]
fn detect_returns_none_on_empty_repo() {
    let dir = fixture();
    assert!(detect(dir.path()).is_none());
}

#[test]
fn detect_returns_none_on_missing_path() {
    let missing = std::path::PathBuf::from("/this/path/does/not/exist/wisetree-presets");
    assert!(detect(&missing).is_none());
}

#[test]
fn malformed_package_json_does_not_panic() {
    let dir = fixture();
    write(dir.path(), "package.json", "{ this is not json");
    // Falls through to generic Express-node detection because
    // file_exists still matches even if file_contains does not.
    assert_eq!(detect(dir.path()), Some(PresetId::ExpressNode));
}

#[test]
fn catalog_includes_generic_fallback() {
    let ids: Vec<_> = catalog().iter().map(|p| p.id).collect();
    assert!(ids.contains(&PresetId::Generic));
}

#[test]
fn catalog_ids_are_unique() {
    let ids: Vec<_> = catalog().iter().map(|p| p.id).collect();
    let mut sorted_ids = ids.clone();
    sorted_ids.sort_by_key(|id| id.as_str());
    sorted_ids.dedup();
    assert_eq!(ids.len(), sorted_ids.len(), "duplicate preset ids");
}
