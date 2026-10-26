use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{backend::TestBackend, Terminal};
use wisetree::messages;
use wisetree::services::global_dashboard::github::{
    AccountDiscovery, AccountKind, GitHubAccount, GitHubError,
};
use wisetree::tui::screens::global_accounts::{GlobalAccountsAction, GlobalAccountsScreen};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn type_query(screen: &mut GlobalAccountsScreen, query: &str) {
    for ch in query.chars() {
        screen.handle_key(key(KeyCode::Char(ch)));
    }
}

fn discovery(accounts: &[&str], warnings: Vec<GitHubError>) -> AccountDiscovery {
    AccountDiscovery {
        viewer: "Victorcorcos".into(),
        accounts: accounts
            .iter()
            .map(|login| GitHubAccount {
                login: (*login).into(),
                kind: AccountKind::Organization,
            })
            .collect(),
        warnings,
    }
}

fn render(screen: &GlobalAccountsScreen, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| screen.render(frame, frame.area()))
        .unwrap();
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

fn ready() -> GlobalAccountsScreen {
    let mut screen = GlobalAccountsScreen::new(vec!["oxeanbits".into(), "gone-account".into()]);
    screen.set_discovery(Ok(discovery(
        &["OXEANBITS", "Victorcorcos", "other"],
        vec![],
    )));
    screen
}

#[test]
fn loading_then_checkboxes_search_and_key_hints_render() {
    let mut screen = GlobalAccountsScreen::new(vec!["oxeanbits".into()]);
    assert!(render(&screen, 100, 20).contains(messages::GLOBAL_ACCOUNTS_LOADING));
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        GlobalAccountsAction::Continue
    );
    screen.set_discovery(Ok(discovery(&["oxeanbits", "Victorcorcos"], vec![])));
    let output = render(&screen, 100, 20);
    for text in [
        messages::GLOBAL_ACCOUNTS_TITLE,
        "Search:",
        "☒ oxeanbits",
        "☐ Victorcorcos",
        "Space toggle",
        "↑↓ navigate",
        "Enter open",
        "Esc back",
        "Ctrl+R retry",
    ] {
        assert!(output.contains(text), "{text}: {output}");
    }
    assert!(!output.contains(messages::GLOBAL_ACCOUNTS_LOADING));
}

#[test]
fn saved_accounts_are_checked_and_retained_when_not_discovered() {
    let screen = ready();
    let output = render(&screen, 100, 20);
    assert!(output.contains("☒ oxeanbits"));
    assert!(output.contains("☒ gone-account"));
    assert!(!output.contains("☐ OXEANBITS"));
    assert!(output.contains("☐ Victorcorcos"));
    assert!(output.contains("☐ other"));
    assert_eq!(screen.selected_accounts(), ["oxeanbits", "gone-account"]);
}

#[test]
fn fuzzy_filter_toggles_only_the_visible_account_and_esc_clears_before_back() {
    let mut screen = ready();
    type_query(&mut screen, "vccs");
    assert!(render(&screen, 100, 20).contains("☐ Victorcorcos"));
    screen.handle_key(key(KeyCode::Char(' ')));
    assert_eq!(
        screen.selected_accounts(),
        ["oxeanbits", "gone-account", "Victorcorcos"]
    );
    assert_eq!(
        screen.handle_key(key(KeyCode::Esc)),
        GlobalAccountsAction::Continue
    );
    let output = render(&screen, 100, 20);
    assert!(output.contains("☒ Victorcorcos"));
    type_query(&mut screen, "gone");
    screen.handle_key(key(KeyCode::Char(' ')));
    assert_eq!(screen.selected_accounts(), ["oxeanbits", "Victorcorcos"]);
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        GlobalAccountsAction::Open(vec!["oxeanbits".into(), "Victorcorcos".into()])
    );
    assert_eq!(
        screen.handle_key(key(KeyCode::Esc)),
        GlobalAccountsAction::Continue
    );
    assert_eq!(
        screen.handle_key(key(KeyCode::Esc)),
        GlobalAccountsAction::Back
    );
}

#[test]
fn no_matches_space_does_nothing_and_enter_uses_the_full_selection() {
    let mut screen = ready();
    type_query(&mut screen, "zzzzzzzz");
    assert!(render(&screen, 100, 20).contains("No matching options"));
    screen.handle_key(key(KeyCode::Char(' ')));
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        GlobalAccountsAction::Open(vec!["oxeanbits".into(), "gone-account".into()])
    );
}

#[test]
fn empty_selection_warns_and_stays_on_screen() {
    let mut screen = GlobalAccountsScreen::new(vec![]);
    screen.set_discovery(Ok(discovery(&[], vec![])));
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        GlobalAccountsAction::Continue
    );
    assert!(render(&screen, 100, 20).contains(messages::GLOBAL_ACCOUNTS_REQUIRED));
}

#[test]
fn discovery_errors_and_partial_results_allow_saved_accounts_and_retry() {
    for error in [
        GitHubError::NotInstalled,
        GitHubError::Authentication("expired".into()),
    ] {
        let mut screen = GlobalAccountsScreen::new(vec!["saved".into()]);
        let message = error.to_string();
        screen.set_discovery(Err(error));
        let output = render(&screen, 120, 20);
        assert!(output.contains(&message));
        assert!(output.contains("☒ saved"));
        assert!(output.contains("Ctrl+R retry"));
        assert_eq!(
            screen.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            GlobalAccountsAction::Refresh
        );
        screen.start_loading();
        screen.set_discovery(Ok(discovery(
            &["new"],
            vec![GitHubError::Request("permission denied".into())],
        )));
        let output = render(&screen, 120, 20);
        assert!(output.contains("☒ saved"));
        assert!(output.contains("☐ new"));
        assert!(output.contains("permission denied"));
        assert_eq!(
            screen.handle_key(key(KeyCode::Esc)),
            GlobalAccountsAction::Back
        );
    }
}

#[test]
fn navigation_wraps_and_small_terminals_render() {
    let mut screen = ready();
    screen.handle_key(key(KeyCode::Up));
    screen.handle_key(key(KeyCode::Char(' ')));
    assert!(screen.selected_accounts().contains(&"other".into()));
    screen.handle_key(key(KeyCode::Down));
    screen.handle_key(key(KeyCode::Char(' ')));
    assert!(!screen.selected_accounts().contains(&"oxeanbits".into()));
    for (width, height) in [(40, 10), (15, 5), (1, 1)] {
        render(&screen, width, height);
    }
}

#[test]
fn cached_accounts_open_at_once_and_revalidation_only_adds_new_ones() {
    let mut screen = GlobalAccountsScreen::new(vec!["oxeanbits".into()]);
    screen.set_discovery(Ok(discovery(
        &["oxeanbits", "Victorcorcos"],
        vec![GitHubError::Request("stale warning".into())],
    )));
    screen.handle_key(key(KeyCode::Down));
    screen.handle_key(key(KeyCode::Char(' ')));
    // The background revalidation lands after the user started picking.
    screen.set_discovery(Ok(discovery(&["Victorcorcos", "joined-org"], vec![])));
    let output = render(&screen, 100, 20);
    let rows: Vec<_> = ["☒ oxeanbits", "☒ Victorcorcos", "☐ joined-org"]
        .iter()
        .map(|row| {
            output
                .find(row)
                .unwrap_or_else(|| panic!("{row}: {output}"))
        })
        .collect();
    assert!(rows.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(!output.contains("stale warning"));
    assert_eq!(
        screen.handle_key(key(KeyCode::Enter)),
        GlobalAccountsAction::Open(vec!["oxeanbits".into(), "Victorcorcos".into()])
    );
}
