//! End-to-end smoke + behavior tests for the Setup Project Config screen.

use std::fs;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use tempfile::tempdir;

use wisetree::config::schema::WorktreeConfig;
use wisetree::config::service::ConfigService;
use wisetree::services::presets::{catalog, find_by_id, PresetId};
use wisetree::tui::screens::setup_project::{
    SetupProjectAction, SetupProjectScreen, SetupProjectStep,
};
use wisetree::tui::widgets::ConfirmChoice;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent {
        code,
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    }
}

fn dump<F>(width: u16, height: u16, draw: F) -> String
where
    F: FnOnce(&mut ratatui::Frame),
{
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(draw).unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect()
}

#[test]
fn preset_list_renders_intro_and_didactic_footer() {
    let s = SetupProjectScreen::new(None);
    let dumped = dump(120, 30, |f| s.render(f, f.area()));
    assert!(dumped.contains("Pick a project preset"));
    assert!(dumped.contains(".wisetree.json"));
    assert!(dumped.contains("Confirming will replace"));
    assert!(dumped.contains("Type to filter"));
    assert!(dumped.contains("Esc to clear"));
}

#[test]
fn auto_detect_preselects_matching_preset() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("Gemfile"), "").unwrap();
    fs::create_dir_all(tmp.path().join("config")).unwrap();
    fs::write(tmp.path().join("config/application.rb"), "").unwrap();

    let s = SetupProjectScreen::new(Some(tmp.path()));
    assert_eq!(s.detected(), Some(PresetId::RubyOnRails));
    assert_eq!(s.selected_preset(), PresetId::RubyOnRails);

    let dumped = dump(120, 40, |f| s.render(f, f.area()));
    assert!(dumped.contains("Ruby on Rails"));
    assert!(dumped.contains("detected"));
}

#[test]
fn esc_on_preset_list_cancels() {
    let mut s = SetupProjectScreen::new(None);
    let action = s.handle_key(key(KeyCode::Esc));
    assert_eq!(action, SetupProjectAction::Cancelled);
}

#[test]
fn enter_on_preset_list_advances_to_confirm() {
    let mut s = SetupProjectScreen::new(None);
    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(action, SetupProjectAction::Continue);
    assert_eq!(s.step(), SetupProjectStep::Confirm);
    assert_eq!(s.confirm_choice(), ConfirmChoice::Confirm);
}

#[test]
fn confirm_renders_three_blocks_and_yes_no() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("Gemfile"), "").unwrap();
    fs::create_dir_all(tmp.path().join("config")).unwrap();
    fs::write(tmp.path().join("config/application.rb"), "").unwrap();

    let mut s = SetupProjectScreen::new(Some(tmp.path()));
    s.handle_key(key(KeyCode::Enter));

    let dumped = dump(120, 50, |f| s.render(f, f.area()));
    assert!(dumped.contains("worktreeCopyPatterns"));
    assert!(dumped.contains("worktreeCopyIgnores"));
    assert!(dumped.contains("postCreateCmd"));
    assert!(dumped.contains("bundle install"));
    assert!(dumped.contains("Yes"));
    assert!(dumped.contains("No"));
}

#[test]
fn esc_on_confirm_returns_to_preset_list() {
    let mut s = SetupProjectScreen::new(None);
    s.handle_key(key(KeyCode::Enter));
    assert_eq!(s.step(), SetupProjectStep::Confirm);

    let action = s.handle_key(key(KeyCode::Esc));
    assert_eq!(action, SetupProjectAction::Continue);
    assert_eq!(s.step(), SetupProjectStep::PresetList);
}

#[test]
fn confirm_yes_default_returns_apply_with_selected_preset() {
    let mut s = SetupProjectScreen::new(None);
    s.handle_key(key(KeyCode::Enter));
    let selected = s.selected_preset();

    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(action, SetupProjectAction::Apply(selected));
}

#[test]
fn arrow_toggles_yes_no_and_no_returns_to_preset_list() {
    let mut s = SetupProjectScreen::new(None);
    s.handle_key(key(KeyCode::Enter));

    s.handle_key(key(KeyCode::Right));
    assert_eq!(s.confirm_choice(), ConfirmChoice::Cancel);

    let action = s.handle_key(key(KeyCode::Enter));
    assert_eq!(action, SetupProjectAction::Continue);
    assert_eq!(s.step(), SetupProjectStep::PresetList);
}

#[test]
fn n_shortcut_selects_no() {
    let mut s = SetupProjectScreen::new(None);
    s.handle_key(key(KeyCode::Enter));
    s.handle_key(key(KeyCode::Char('n')));
    assert_eq!(s.confirm_choice(), ConfirmChoice::Cancel);

    s.handle_key(key(KeyCode::Char('y')));
    assert_eq!(s.confirm_choice(), ConfirmChoice::Confirm);
}

#[test]
fn end_to_end_apply_writes_local_config_with_preset_values() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("Gemfile"), "").unwrap();
    fs::create_dir_all(tmp.path().join("config")).unwrap();
    fs::write(tmp.path().join("config/application.rb"), "").unwrap();

    let mut s = SetupProjectScreen::new(Some(tmp.path()));
    assert_eq!(s.detected(), Some(PresetId::RubyOnRails));

    // First Enter advances PresetList → Confirm (Continue, not Apply).
    assert_eq!(
        s.handle_key(key(KeyCode::Enter)),
        SetupProjectAction::Continue
    );
    assert_eq!(s.step(), SetupProjectStep::Confirm);

    // Second Enter (Yes is default) emits Apply with the chosen preset.
    let action = s.handle_key(key(KeyCode::Enter));
    let preset_id = match action {
        SetupProjectAction::Apply(id) => id,
        other => panic!("expected Apply, got {:?}", other),
    };
    assert_eq!(preset_id, PresetId::RubyOnRails);

    let preset = find_by_id(preset_id).expect("preset exists");
    let config = WorktreeConfig {
        worktree_copy_patterns: preset.copy_patterns_owned(),
        worktree_copy_ignores: preset.copy_ignores_owned(),
        post_create_cmd: preset.post_create_cmd_owned(),
        ..WorktreeConfig::default()
    };

    let mut service = ConfigService::new();
    let local_path = tmp.path().join(".wisetree.json");
    service.save(&config, Some(&local_path)).unwrap();

    let written: WorktreeConfig =
        serde_json::from_str(&fs::read_to_string(&local_path).unwrap()).unwrap();
    assert!(written
        .worktree_copy_patterns
        .iter()
        .any(|p| p == "config/master.key"));
    assert!(written
        .worktree_copy_ignores
        .iter()
        .any(|p| p == "**/vendor/bundle/**"));
    assert!(written
        .post_create_cmd
        .iter()
        .any(|c| c == "bundle install --jobs 5 --verbose --retry 4"));
}

#[test]
fn catalog_contains_top_frameworks() {
    let labels: Vec<&'static str> = catalog().iter().map(|p| p.label).collect();
    for expected in [
        "Ruby on Rails",
        "Django",
        "React (CRA / Vite)",
        "Next.js",
        "Flutter / Dart",
        "Go",
        "Rust / Cargo",
    ] {
        assert!(labels.contains(&expected), "missing preset: {expected}");
    }
}
