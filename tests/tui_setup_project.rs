//! End-to-end smoke + behavior tests for the Setup Project Config screen.

use std::fs;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use tempfile::tempdir;

use wisetree::config::schema::WorktreeConfig;
use wisetree::config::service::ConfigService;
use wisetree::services::presets::{catalog, discover_wise, PresetId};
use wisetree::tui::screens::setup_project::{
    PresetChoice, SetupProjectAction, SetupProjectScreen, SetupProjectStep,
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
    let screen = SetupProjectScreen::new(None);
    let dumped = dump(120, 30, |frame| screen.render(frame, frame.area()));
    assert!(dumped.contains("Pick a project preset"));
    assert!(dumped.contains(".wisetree.json"));
    assert!(dumped.contains("Confirming will replace"));
    assert!(dumped.contains("Type to filter"));
    assert!(dumped.contains("Esc to clear"));
}

#[test]
fn preset_list_includes_wise_preset_and_defaults_to_it_without_root_match() {
    let screen = SetupProjectScreen::new(None);
    let dumped = dump(120, 30, |frame| screen.render(frame, frame.area()));

    assert!(dumped.contains("1. Wise Preset"));
    assert_eq!(screen.selected_choice(), PresetChoice::Wise);
    assert_eq!(screen.selected_preset(), None);
}

#[test]
fn auto_detect_preselects_matching_catalog_preset() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("Gemfile"), "").unwrap();
    fs::create_dir_all(tmp.path().join("config")).unwrap();
    fs::write(tmp.path().join("config/application.rb"), "").unwrap();

    let screen = SetupProjectScreen::new(Some(tmp.path()));
    assert_eq!(screen.detected(), Some(PresetId::RubyOnRails));
    assert_eq!(
        screen.selected_choice(),
        PresetChoice::Catalog(PresetId::RubyOnRails)
    );
    assert_eq!(screen.selected_preset(), Some(PresetId::RubyOnRails));

    let dumped = dump(120, 40, |frame| screen.render(frame, frame.area()));
    assert!(dumped.contains("Ruby on Rails"));
    assert!(dumped.contains("detected"));
}

#[test]
fn esc_on_preset_list_cancels() {
    let mut screen = SetupProjectScreen::new(None);
    let action = screen.handle_key(key(KeyCode::Esc));
    assert_eq!(action, SetupProjectAction::Cancelled);
}

#[test]
fn enter_on_wise_preset_starts_discovery() {
    let mut screen = SetupProjectScreen::new(None);
    let action = screen.handle_key(key(KeyCode::Enter));

    assert_eq!(action, SetupProjectAction::DiscoverWise);
    assert_eq!(screen.step(), SetupProjectStep::Discovering);

    let dumped = dump(120, 20, |frame| screen.render(frame, frame.area()));
    assert!(dumped.contains("Wise Preset is researching the repository"));
}

#[test]
fn enter_on_catalog_preset_advances_to_confirm() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("Gemfile"), "").unwrap();
    fs::create_dir_all(tmp.path().join("config")).unwrap();
    fs::write(tmp.path().join("config/application.rb"), "").unwrap();

    let mut screen = SetupProjectScreen::new(Some(tmp.path()));
    let action = screen.handle_key(key(KeyCode::Enter));
    assert_eq!(action, SetupProjectAction::Continue);
    assert_eq!(screen.step(), SetupProjectStep::Confirm);
    assert_eq!(screen.confirm_choice(), ConfirmChoice::Confirm);
}

#[test]
fn wise_discovery_completion_renders_three_blocks_and_yes_no() {
    let tmp = tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("api")).unwrap();
    fs::write(tmp.path().join("api/Gemfile"), "").unwrap();
    fs::create_dir_all(tmp.path().join("api/config")).unwrap();
    fs::write(tmp.path().join("api/config/application.rb"), "").unwrap();
    fs::write(tmp.path().join("api/config/master.key"), "secret").unwrap();
    fs::create_dir_all(tmp.path().join("web")).unwrap();
    fs::write(
        tmp.path().join("web/package.json"),
        "{\"dependencies\": {\"react\": \"18\"}}",
    )
    .unwrap();
    fs::write(tmp.path().join("web/.env.local"), "VITE_X=1").unwrap();

    let mut screen = SetupProjectScreen::new(Some(tmp.path()));
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        SetupProjectAction::DiscoverWise
    );
    screen.complete_wise_discovery(discover_wise(tmp.path()).expect("wise preset"));

    let dumped = dump(120, 60, |frame| screen.render(frame, frame.area()));
    assert!(dumped.contains("Apply Wise Preset to .wisetree.json?"));
    assert!(dumped.contains("worktreeCopyPatterns"));
    assert!(dumped.contains("worktreeCopyIgnores"));
    assert!(dumped.contains("postCreateCmd"));
    assert!(dumped.contains("api/config/master.key"));
    assert!(dumped.contains("web/.env.local"));
    assert!(dumped.contains("(cd 'api' && bundle install"));
    assert!(dumped.contains("(cd 'web' && npm install)"));
    assert!(dumped.contains("Yes"));
    assert!(dumped.contains("No"));
}

#[test]
fn esc_on_confirm_returns_to_preset_list() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("Gemfile"), "").unwrap();
    fs::create_dir_all(tmp.path().join("config")).unwrap();
    fs::write(tmp.path().join("config/application.rb"), "").unwrap();

    let mut screen = SetupProjectScreen::new(Some(tmp.path()));
    screen.handle_key(key(KeyCode::Enter));
    assert_eq!(screen.step(), SetupProjectStep::Confirm);

    let action = screen.handle_key(key(KeyCode::Esc));
    assert_eq!(action, SetupProjectAction::Continue);
    assert_eq!(screen.step(), SetupProjectStep::PresetList);
}

#[test]
fn confirm_yes_default_returns_apply_with_selected_catalog_values() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("Gemfile"), "").unwrap();
    fs::create_dir_all(tmp.path().join("config")).unwrap();
    fs::write(tmp.path().join("config/application.rb"), "").unwrap();

    let mut screen = SetupProjectScreen::new(Some(tmp.path()));
    screen.handle_key(key(KeyCode::Enter));

    let action = screen.handle_key(key(KeyCode::Enter));
    let values = match action {
        SetupProjectAction::Apply(values) => values,
        other => panic!("expected Apply, got {other:?}"),
    };

    assert_eq!(values.label, "Ruby on Rails");
    assert!(values
        .post_create_cmd
        .iter()
        .any(|command| command == "bundle install --jobs 5 --verbose --retry 4"));
}

#[test]
fn arrow_toggles_yes_no_and_no_returns_to_preset_list() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("Gemfile"), "").unwrap();
    fs::create_dir_all(tmp.path().join("config")).unwrap();
    fs::write(tmp.path().join("config/application.rb"), "").unwrap();

    let mut screen = SetupProjectScreen::new(Some(tmp.path()));
    screen.handle_key(key(KeyCode::Enter));

    screen.handle_key(key(KeyCode::Right));
    assert_eq!(screen.confirm_choice(), ConfirmChoice::Cancel);

    let action = screen.handle_key(key(KeyCode::Enter));
    assert_eq!(action, SetupProjectAction::Continue);
    assert_eq!(screen.step(), SetupProjectStep::PresetList);
}

#[test]
fn n_shortcut_selects_no() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("Gemfile"), "").unwrap();
    fs::create_dir_all(tmp.path().join("config")).unwrap();
    fs::write(tmp.path().join("config/application.rb"), "").unwrap();

    let mut screen = SetupProjectScreen::new(Some(tmp.path()));
    screen.handle_key(key(KeyCode::Enter));
    screen.handle_key(key(KeyCode::Char('n')));
    assert_eq!(screen.confirm_choice(), ConfirmChoice::Cancel);

    screen.handle_key(key(KeyCode::Char('y')));
    assert_eq!(screen.confirm_choice(), ConfirmChoice::Confirm);
}

#[test]
fn end_to_end_apply_writes_local_config_with_wise_values() {
    let tmp = tempdir().unwrap();
    fs::create_dir_all(tmp.path().join("api")).unwrap();
    fs::write(tmp.path().join("api/Gemfile"), "").unwrap();
    fs::create_dir_all(tmp.path().join("api/config")).unwrap();
    fs::write(tmp.path().join("api/config/application.rb"), "").unwrap();
    fs::write(tmp.path().join("api/config/master.key"), "secret").unwrap();
    fs::create_dir_all(tmp.path().join("web")).unwrap();
    fs::write(
        tmp.path().join("web/package.json"),
        "{\"dependencies\": {\"react\": \"18\"}}",
    )
    .unwrap();
    fs::write(tmp.path().join("web/.env.local"), "VITE_X=1").unwrap();

    let mut screen = SetupProjectScreen::new(Some(tmp.path()));
    assert_eq!(screen.selected_choice(), PresetChoice::Wise);
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        SetupProjectAction::DiscoverWise
    );
    screen.complete_wise_discovery(discover_wise(tmp.path()).expect("wise preset"));

    let values = match screen.handle_key(key(KeyCode::Enter)) {
        SetupProjectAction::Apply(values) => values,
        other => panic!("expected Apply, got {other:?}"),
    };

    let config = WorktreeConfig {
        worktree_copy_patterns: values.copy_patterns,
        worktree_copy_ignores: values.copy_ignores,
        post_create_cmd: values.post_create_cmd,
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
        .any(|pattern| pattern == "api/config/master.key"));
    assert!(written
        .worktree_copy_patterns
        .iter()
        .any(|pattern| pattern == "web/.env.local"));
    assert!(written
        .worktree_copy_ignores
        .iter()
        .any(|pattern| pattern == "api/**/vendor/bundle/**"));
    assert!(written
        .worktree_copy_ignores
        .iter()
        .any(|pattern| pattern == "web/**/node_modules/**"));
    assert!(written
        .post_create_cmd
        .iter()
        .any(|command| command == "(cd 'api' && bundle install --jobs 5 --verbose --retry 4)"));
    assert!(written
        .post_create_cmd
        .iter()
        .any(|command| command == "(cd 'web' && npm install)"));
}

#[test]
fn catalog_contains_top_frameworks() {
    let labels: Vec<&'static str> = catalog().iter().map(|preset| preset.label).collect();
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
