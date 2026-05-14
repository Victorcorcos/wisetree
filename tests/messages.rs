use wisetree::messages;

#[test]
fn welcome_message_uses_wisetree_branding() {
    assert!(messages::WELCOME.contains("Wisetree"));
    assert!(!messages::WELCOME.is_empty());
}

#[test]
fn key_user_facing_strings_exist() {
    assert!(!messages::MENU_TITLE.is_empty());
    assert!(!messages::CREATE_CONFIRM_TITLE.is_empty());
    assert!(!messages::DELETE_CONFIRM_TITLE.is_empty());
    assert!(!messages::ERROR_NOT_GIT_REPO.is_empty());
    assert_eq!(messages::UPDATE_INSTALL_CMD, "npm install -g wisetree");
}

#[test]
fn loading_states_are_distinct() {
    let loading = [
        messages::LOADING_GIT_INFO,
        messages::LOADING_BRANCHES,
        messages::LOADING_WORKTREES,
    ];
    let mut seen = std::collections::HashSet::new();
    for s in loading {
        assert!(!s.is_empty());
        assert!(seen.insert(s), "duplicate loading message: {s}");
    }
}
