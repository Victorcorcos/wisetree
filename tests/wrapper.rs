//! Wrapper-mode behavior tests. The full end-to-end scenario (PTY harness
//! → captured stdout) requires `script(1)` / a Unix-only PTY library; here
//! we cover the contract that doesn't need a real TTY:
//!
//! - `App::new(_, true)` exposes `is_from_wrapper = true`.
//! - A freshly-constructed app reports no `selected_path`.
//! - The `terminal::enter_wrapper` symbol is reachable (compile-time check).

use wisetree::cli::AppMode;
use wisetree::tui::App;

#[test]
fn wrapper_flag_propagates_to_app() {
    let app = App::new(AppMode::Dashboard, true);
    assert!(app.is_from_wrapper);
    assert!(app.selected_path().is_none());
}

#[test]
fn non_wrapper_app_is_marked_correctly() {
    let app = App::new(AppMode::Menu, false);
    assert!(!app.is_from_wrapper);
    assert!(app.selected_path().is_none());
}

#[test]
fn wrapper_terminal_constructor_is_callable() {
    // Compile-time check: the symbol exists and has the expected
    // signature. We don't actually open `/dev/tty` here because cargo
    // test is not guaranteed a controlling terminal.
    let _: fn() -> std::io::Result<wisetree::tui::terminal::WrapperTerminal> =
        wisetree::tui::terminal::enter_wrapper;
}
