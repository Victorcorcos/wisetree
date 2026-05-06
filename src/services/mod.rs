//! Cross-cutting services (app state, update check, shell integration).

pub mod app_state;
pub mod shell_integration;
pub mod update;

pub use app_state::AppStateService;
pub use shell_integration::{
    detect_shell, detect_shell_integration, generate_setup_block, get_config_path,
    install_shell_integration, remove_shell_integration, Shell, ShellIntegrationStatus,
};
pub use update::{
    check_for_updates, get_cached_update_status, should_check_for_updates, UpdateCheckResult,
};
