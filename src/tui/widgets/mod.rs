//! Reusable TUI primitives. Stateful widgets (input/select/confirm) own
//! their state and expose a `handle_key` method returning an outcome enum;
//! purely presentational widgets (spinner / status / command-list) are
//! drawn directly from the caller's state.

pub mod border;
pub mod command_list_progress;
pub mod command_progress;
pub mod confirm_dialog;
pub mod input_prompt;
pub mod select_prompt;
pub mod spinner;
pub mod status_indicator;
pub mod update_banner;
pub mod welcome_header;

pub use border::BorderState;
pub use command_list_progress::CommandListProgress;
pub use command_progress::CommandProgress;
pub use confirm_dialog::{ConfirmChoice, ConfirmDialog, ConfirmOutcome, ConfirmVariant};
pub use input_prompt::{InputOutcome, InputPrompt};
pub use select_prompt::{SelectOption, SelectOutcome, SelectPrompt, SelectStyle, SELECT_CURSOR};
pub use spinner::{spinner_frame, Spinner, SPINNER_FRAMES};
pub use status_indicator::{Status, StatusIndicator};
pub use update_banner::UpdateBanner;
pub use welcome_header::WelcomeHeader;
